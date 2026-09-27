//! Ways of reaching a telescope's storage. Telescope modules scan through the
//! [`Transport`] trait, so they work the same over Wi-Fi and USB-C.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub trait Transport {
    /// List a directory given relative to the transport root ("" = root, `/`-separated).
    fn list(&mut self, dir: &str) -> Result<Vec<Entry>>;
    /// Copy a remote file to a local path.
    fn fetch(&mut self, path: &str, local: &Path) -> Result<()>;
    /// Delete a remote file.
    fn delete(&mut self, path: &str) -> Result<()>;

    /// Whether `dir` exists and can be listed.
    fn exists(&mut self, dir: &str) -> bool {
        self.list(dir).is_ok()
    }
}

/// Join a relative remote path.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

pub fn is_fits(name: &str) -> bool {
    let n = name.to_lowercase();
    n.ends_with(".fit") || n.ends_with(".fits") || n.ends_with(".fts")
}

pub fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow!("no IPv4 address for {host}"))
}

/// A mounted drive or any local folder (telescopes connected over USB-C).
pub struct LocalTransport {
    pub root: PathBuf,
}

impl Transport for LocalTransport {
    fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.root.join(dir))? {
            let e = e?;
            let md = e.metadata()?;
            out.push(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: md.is_dir(),
                size: md.len(),
            });
        }
        Ok(out)
    }
    fn fetch(&mut self, path: &str, local: &Path) -> Result<()> {
        std::fs::copy(self.root.join(path), local)?;
        Ok(())
    }
    fn delete(&mut self, path: &str) -> Result<()> {
        std::fs::remove_file(self.root.join(path))?;
        Ok(())
    }
}

/// SMB2/3 share (pure Rust, works on every OS).
pub struct SmbTransport {
    rt: tokio::runtime::Runtime,
    client: smb2::SmbClient,
    tree: smb2::Tree,
}

impl SmbTransport {
    pub fn connect(host: &str, user: &str, password: &str, share: &str) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let addr = resolve(host, 445)?;
        let (client, tree) = rt
            .block_on(async {
                let mut client = smb2::connect(&addr.to_string(), user, password).await?;
                let tree = client.connect_share(share).await?;
                Ok::<_, smb2::Error>((client, tree))
            })
            .map_err(|e| anyhow!("SMB connection to {host} failed: {e}"))?;
        Ok(SmbTransport { rt, client, tree })
    }
}

impl Transport for SmbTransport {
    fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let entries = self
            .rt
            .block_on(self.client.list_directory(&mut self.tree, dir))
            .map_err(|e| anyhow!("listing {dir}: {e}"))?;
        Ok(entries
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .map(|e| Entry {
                name: e.name,
                is_dir: e.is_directory,
                size: e.size,
            })
            .collect())
    }
    fn fetch(&mut self, path: &str, local: &Path) -> Result<()> {
        let SmbTransport { rt, client, tree } = self;
        rt.block_on(async {
            let mut dl = client
                .download(tree, path)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let mut out = std::io::BufWriter::new(std::fs::File::create(local)?);
            while let Some(chunk) = dl.next_chunk().await {
                out.write_all(&chunk.map_err(|e| anyhow!("{e}"))?)?;
            }
            out.flush()?;
            Ok(())
        })
    }
    fn delete(&mut self, path: &str) -> Result<()> {
        self.rt
            .block_on(self.client.delete_file(&mut self.tree, path))
            .map_err(|e| anyhow!("deleting {path}: {e}"))
    }
}

/// Plain FTP.
pub struct FtpTransport {
    ftp: suppaftp::FtpStream,
}

impl FtpTransport {
    pub fn connect(host: &str, user: &str, password: &str) -> Result<Self> {
        let addr = resolve(host, 21)?;
        let mut ftp = suppaftp::FtpStream::connect_timeout(addr, Duration::from_secs(10))
            .map_err(|e| anyhow!("FTP connection to {host} failed: {e}"))?;
        ftp.login(user, password)
            .map_err(|e| anyhow!("FTP login failed: {e}"))?;
        ftp.transfer_type(suppaftp::types::FileType::Binary)?;
        ftp.get_ref()
            .set_read_timeout(Some(Duration::from_secs(60)))
            .ok();
        Ok(FtpTransport { ftp })
    }
}

impl Transport for FtpTransport {
    fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let path = format!("/{dir}");
        let lines = self
            .ftp
            .list(Some(&path))
            .map_err(|e| anyhow!("listing {path}: {e}"))?;
        let mut out = Vec::new();
        for line in lines {
            if let Ok(f) = suppaftp::list::File::try_from(line.as_str()) {
                let name = f.name().to_string();
                if name != "." && name != ".." {
                    out.push(Entry {
                        name,
                        is_dir: f.is_directory(),
                        size: f.size() as u64,
                    });
                }
            }
        }
        Ok(out)
    }
    fn fetch(&mut self, path: &str, local: &Path) -> Result<()> {
        let mut stream = self
            .ftp
            .retr_as_stream(format!("/{path}"))
            .map_err(|e| anyhow!("retrieving {path}: {e}"))?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(local)?);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        self.ftp.finalize_retr_stream(stream)?;
        Ok(())
    }
    fn delete(&mut self, path: &str) -> Result<()> {
        self.ftp
            .rm(format!("/{path}"))
            .map_err(|e| anyhow!("deleting {path}: {e}"))
    }
}

impl Drop for FtpTransport {
    fn drop(&mut self) {
        let _ = self.ftp.quit();
    }
}
