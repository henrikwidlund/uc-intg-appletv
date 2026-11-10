// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! HTTP route for artwork that has no URL.
//!
//! The image from the Apple TV is written to a file. The last images of each device are kept,
//! so an image that comes again is not written again. The file name comes from the artwork id,
//! so the Remote can cache the image.

use crate::config_store::fnv1a;
use actix_web::http::header;
use actix_web::{HttpResponse, get, web};
use log::{debug, warn};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Time the Remote may cache an image. A file name always has the same image.
const CACHE_CONTROL: &str = "max-age=86400";

/// Images kept per device.
const KEEP: usize = 16;

/// Image files per device, shared between the device actors and the HTTP route.
#[derive(Debug, Clone)]
pub struct ArtworkStore {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    dir: PathBuf,
    files: Mutex<Files>,
    /// Makes the temporary file names unique.
    tmp_seq: AtomicU64,
}

#[derive(Debug, Default)]
struct Files {
    /// File names per device, the oldest first.
    names: HashMap<Box<str>, VecDeque<Box<str>>>,
    /// Number of [`ArtworkStore::remove`] calls per device. A save that sees a change does not
    /// keep its file.
    removed: HashMap<Box<str>, u64>,
}

impl ArtworkStore {
    /// Use `dir` for the image files. Files from an earlier run are removed.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("Could not remove old artwork in {}: {e}", dir.display());
        }
        Self {
            inner: Arc::new(Inner {
                dir,
                files: Mutex::default(),
                tmp_seq: AtomicU64::new(0),
            }),
        }
    }

    /// Write the image of a device, unless it is kept already. The oldest image is removed
    /// when more than [`KEEP`] are kept. Returns the file name.
    pub async fn save(
        &self,
        device: &str,
        artwork_id: &str,
        mime_type: Option<&str>,
        data: &[u8],
    ) -> io::Result<String> {
        let ext = mime_type.map_or_else(|| sniff(data), extension);
        let name = format!("{:016x}.{ext}", fnv1a(artwork_id.as_bytes()));
        let removed = {
            let mut files = self.lock();
            if touch(&mut files, device, &name) {
                return Ok(name);
            }
            files.removed.get(device).copied().unwrap_or_default()
        };
        tokio::fs::create_dir_all(&self.inner.dir).await?;
        let path = self.path(device, &name);
        let seq = self.inner.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = self.inner.dir.join(format!("{device}-{seq}.tmp"));
        if let Err(e) = write(&tmp, &path, data).await {
            remove_files(vec![tmp]);
            return Err(e);
        }
        let mut old = Vec::new();
        {
            let mut files = self.lock();
            if files.removed.get(device).copied().unwrap_or_default() != removed {
                drop(files);
                remove_files(vec![path]);
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "device was removed",
                ));
            }
            let list = files.names.entry(device.into()).or_default();
            if !list.iter().any(|n| **n == *name) {
                list.push_back(name.as_str().into());
                while list.len() > KEEP {
                    if let Some(n) = list.pop_front() {
                        old.push(self.path(device, &n));
                    }
                }
            }
        }
        remove_files(old);
        Ok(name)
    }

    /// Mark the image as used now. False if it is not kept.
    pub fn touch(&self, device: &str, name: &str) -> bool {
        touch(&mut self.lock(), device, name)
    }

    /// Remove all images of a device.
    pub fn remove(&self, device: &str) {
        let list = {
            let mut files = self.lock();
            *files.removed.entry(device.into()).or_default() += 1;
            files.names.remove(device)
        };
        if let Some(list) = list {
            remove_files(list.iter().map(|n| self.path(device, n)).collect());
        }
    }

    /// Path of the image if it is kept for the device.
    fn file(&self, device: &str, name: &str) -> Option<PathBuf> {
        let files = self.lock();
        files
            .names
            .get(device)
            .is_some_and(|l| l.iter().any(|n| **n == *name))
            .then(|| self.path(device, name))
    }

    fn path(&self, device: &str, name: &str) -> PathBuf {
        self.inner.dir.join(format!("{device}-{name}"))
    }

    fn lock(&self) -> MutexGuard<'_, Files> {
        // The maps stay valid if a holder panics.
        self.inner
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn touch(files: &mut Files, device: &str, name: &str) -> bool {
    let Some(list) = files.names.get_mut(device) else {
        return false;
    };
    let Some(i) = list.iter().position(|n| **n == *name) else {
        return false;
    };
    if let Some(n) = list.remove(i) {
        list.push_back(n);
    }
    true
}

async fn write(tmp: &Path, path: &Path, data: &[u8]) -> io::Result<()> {
    tokio::fs::write(tmp, data).await?;
    tokio::fs::rename(tmp, path).await
}

/// Delete the files. Call it without the lock.
///
/// The delete is done before this returns, so a later save of the same name cannot be deleted
/// by it. Deleting a file is fast.
fn remove_files(paths: Vec<PathBuf>) {
    for path in paths {
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("Could not remove {}: {e}", path.display());
        }
    }
}

/// File extension from the first bytes of an image. JPEG if unknown.
fn sniff(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "png"
    } else {
        "jpg"
    }
}

fn extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/heic" => "heic",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

fn mime_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("png") => "image/png",
        Some("heic") => "image/heic",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

/// Serve a kept image of a device.
#[get("/artwork/{device}/{name}")]
pub async fn artwork_index(
    path: web::Path<(String, String)>,
    store: web::Data<ArtworkStore>,
) -> HttpResponse {
    let (device, name) = path.into_inner();
    // Only known names are served, so the path cannot leave the directory.
    let Some(file) = store.file(&device, &name) else {
        debug!("Artwork {device}/{name} is not kept");
        return HttpResponse::NotFound().finish();
    };
    tokio::fs::read(&file).await.map_or_else(
        |_| HttpResponse::NotFound().finish(),
        |data| {
            HttpResponse::Ok()
                .insert_header((header::CONTENT_TYPE, mime_type(&name)))
                .insert_header((header::CACHE_CONTROL, CACHE_CONTROL))
                .body(data)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn last_images_are_kept() {
        let dir = std::env::temp_dir().join(format!("uc-intg-appletv-art-{}", std::process::id()));
        let store = ArtworkStore::new(&dir);
        let a = store
            .save("dev", "id-a", Some("image/png"), b"png")
            .await
            .expect("save");
        let path_a = store.file("dev", &a).expect("kept");
        assert!(store.file("dev", "../x").is_none());

        // The same artwork id gives the same name, so the Remote can use its cache.
        let again = store
            .save("dev", "id-a", Some("image/png"), b"other")
            .await
            .expect("save");
        assert_eq!(a, again);
        assert_eq!(b"png".as_slice(), std::fs::read(&path_a).expect("read"));

        let b = store.save("dev", "id-b", None, b"jpg").await.expect("save");
        assert_ne!(a, b);
        assert!(path_a.exists());
        assert_eq!("image/jpeg", mime_type(&b));

        // `a` is used again, so `b` is the oldest and is removed first.
        assert!(store.touch("dev", &a));
        for i in 0..KEEP - 1 {
            store
                .save("dev", &format!("id-{i}"), None, b"x")
                .await
                .expect("save");
        }
        assert!(store.file("dev", &b).is_none());
        assert!(store.file("dev", &a).is_some());

        store.remove("dev");
        assert!(store.file("dev", &a).is_none());
        assert!(!path_a.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
