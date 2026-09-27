//! Folder dialogs that don't block the interface.
//!
//! A native dialog shown from the UI thread stops the window from redrawing
//! and answering the desktop's "are you alive?" pings, so GNOME reports the
//! app as not responding while the user browses. The dialog runs on its own
//! thread instead and the chosen folder is collected on a later frame.

use eframe::egui;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Default)]
pub struct FolderPicker {
    pending: Option<(&'static str, Receiver<Option<String>>)>,
    done: Option<(&'static str, String)>,
}

impl FolderPicker {
    /// Show a folder dialog for the field `key`, starting in `start` if it is
    /// a folder. Ignored while another dialog is open.
    pub fn open(&mut self, key: &'static str, start: &str, ctx: &egui::Context) {
        if self.is_open() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let mut dialog = rfd::AsyncFileDialog::new();
        if Path::new(start).is_dir() {
            dialog = dialog.set_directory(start);
        }
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let chosen = tokio::runtime::Builder::new_current_thread()
                .build()
                .ok()
                .and_then(|rt| rt.block_on(dialog.pick_folder()))
                .map(|h| h.path().to_string_lossy().into_owned());
            tx.send(chosen).ok();
            ctx.request_repaint();
        });
        self.pending = Some((key, rx));
    }

    pub fn is_open(&mut self) -> bool {
        self.poll();
        self.pending.is_some()
    }

    /// The folder chosen for `key`, once, after its dialog closes.
    pub fn take(&mut self, key: &'static str) -> Option<String> {
        self.poll();
        match &self.done {
            Some((k, _)) if *k == key => self.done.take().map(|(_, p)| p),
            _ => None,
        }
    }

    fn poll(&mut self) {
        if let Some((key, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(chosen) => {
                    // A folder picked through GNOME's network view is read
                    // through the kernel mount of the share, if there is one.
                    self.done = chosen.map(|p| {
                        let fast = crate::util::prefer_kernel_mount(Path::new(&p));
                        (*key, fast.to_string_lossy().into_owned())
                    });
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }
    }
}
