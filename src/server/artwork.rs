// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! HTTP route for artwork that has no URL.
//!
//! The image from the Apple TV is kept in memory. The last images of each device are kept,
//! so an image that comes again is not stored again. The file name comes from the artwork id,
//! so the Remote can cache the image.

use crate::config_store::fnv1a;
use actix_web::http::header;
use actix_web::{HttpResponse, get, web};
use bytes::Bytes;
use log::{debug, warn};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Time the Remote may cache an image. A file name always has the same image.
const CACHE_CONTROL: &str = "max-age=86400";

/// Images kept per device.
const KEEP: usize = 16;

/// Images per device, shared between the device actors and the HTTP route.
#[derive(Debug, Clone, Default)]
pub struct ArtworkStore {
    files: Arc<Mutex<HashMap<Box<str>, VecDeque<Image>>>>,
}

/// A kept image. The oldest is first in its list.
#[derive(Debug)]
struct Image {
    name: Box<str>,
    data: Bytes,
}

impl ArtworkStore {
    /// Removes the image files that older versions wrote to `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("Could not remove old artwork in {}: {e}", dir.display());
        }
        Self::default()
    }

    /// Keep the image of a device, unless it is kept already. The oldest image is removed
    /// when more than [`KEEP`] are kept. Returns the file name.
    pub fn save(
        &self,
        device: &str,
        artwork_id: &str,
        mime_type: Option<&str>,
        data: Bytes,
    ) -> Box<str> {
        let ext = mime_type.map_or_else(|| sniff(&data), extension);
        let name: Box<str> = format!("{:016x}.{ext}", fnv1a(artwork_id.as_bytes())).into();
        let mut files = self.lock();
        if touch(&mut files, device, &name) {
            return name;
        }
        let list = files.entry(device.into()).or_default();
        list.push_back(Image {
            name: name.clone(),
            data,
        });
        while list.len() > KEEP {
            list.pop_front();
        }
        name
    }

    /// Mark the image as used now. False if it is not kept.
    pub fn touch(&self, device: &str, name: &str) -> bool {
        touch(&mut self.lock(), device, name)
    }

    /// Remove all images of a device.
    pub fn remove(&self, device: &str) {
        self.lock().remove(device);
    }

    /// The image, if it is kept for the device.
    fn image(&self, device: &str, name: &str) -> Option<Bytes> {
        self.lock()
            .get(device)?
            .iter()
            .find(|i| *i.name == *name)
            .map(|i| i.data.clone())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Box<str>, VecDeque<Image>>> {
        // The map stays valid if a holder panics.
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn touch(files: &mut HashMap<Box<str>, VecDeque<Image>>, device: &str, name: &str) -> bool {
    let Some(list) = files.get_mut(device) else {
        return false;
    };
    let Some(i) = list.iter().position(|img| *img.name == *name) else {
        return false;
    };
    if let Some(img) = list.remove(i) {
        list.push_back(img);
    }
    true
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
    let Some(data) = store.image(&device, &name) else {
        debug!("Artwork {device}/{name} is not kept");
        return HttpResponse::NotFound().finish();
    };
    HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, mime_type(&name)))
        .insert_header((header::CACHE_CONTROL, CACHE_CONTROL))
        .body(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_images_are_kept() {
        let store = ArtworkStore::default();
        let a = store.save("dev", "id-a", Some("image/png"), Bytes::from_static(b"png"));
        assert_eq!(Some(Bytes::from_static(b"png")), store.image("dev", &a));
        assert!(store.image("dev", "../x").is_none());

        // The same artwork id gives the same name, so the Remote can use its cache.
        let again = store.save(
            "dev",
            "id-a",
            Some("image/png"),
            Bytes::from_static(b"other"),
        );
        assert_eq!(a, again);
        assert_eq!(Some(Bytes::from_static(b"png")), store.image("dev", &a));

        let b = store.save("dev", "id-b", None, Bytes::from_static(b"jpg"));
        assert_ne!(a, b);
        assert_eq!("image/jpeg", mime_type(&b));

        // `a` is used again, so `b` is the oldest and is removed first.
        assert!(store.touch("dev", &a));
        for i in 0..KEEP - 1 {
            store.save("dev", &format!("id-{i}"), None, Bytes::from_static(b"x"));
        }
        assert!(store.image("dev", &b).is_none());
        assert!(store.image("dev", &a).is_some());

        store.remove("dev");
        assert!(store.image("dev", &a).is_none());
    }
}
