//! Against the real Photos library, read-only. Needs Photos access for the
//! terminal that runs the tests, and at least one picture from the past year.
//!
//! ```sh
//! cargo test -p tsetsen-macos --test live -- --ignored --test-threads=1
//! ```
//!
//! Exports go to a temporary folder that is deleted afterwards; the viewer
//! window opens for a moment.

use std::time::{Duration, SystemTime};

use tsetsen_core::ports::{PhotoLibrary, PhotoQuery, Showcase};
use tsetsen_core::{Ctx, RequestId};
use tsetsen_macos::{BUILT_HELPER, MacPhotos, MacShowcase};

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(60))
}

fn past_year() -> PhotoQuery {
    PhotoQuery {
        from: SystemTime::now() - Duration::from_hours(8760),
        to: SystemTime::now(),
        limit: 50,
    }
}

#[tokio::test]
#[ignore = "needs Photos access and pictures from the past year"]
async fn finds_exports_and_shows_pictures() {
    let photos = MacPhotos::new(BUILT_HELPER);
    assert_eq!(photos.status(&cx()).await.unwrap(), "authorized");
    let found = photos.search(&past_year(), &cx()).await.unwrap();
    assert!(!found.is_empty(), "no pictures from the past year");
    assert!(
        found.windows(2).all(|w| w[0].taken >= w[1].taken),
        "newest first"
    );

    let ids: Vec<String> = found.iter().take(2).map(|p| p.id.clone()).collect();
    let exported = photos.export(&ids, 800, &cx()).await.unwrap();
    assert_eq!(exported.len(), ids.len());
    for picture in &exported {
        let bytes = std::fs::read(&picture.path).unwrap();
        assert_eq!(&bytes[..2], [0xFF, 0xD8], "a JPEG");
        assert_eq!(bytes.len() as u64, picture.bytes);
    }

    let showcase = MacShowcase::new(BUILT_HELPER, "Tsetsen test");
    let paths: Vec<_> = exported.iter().map(|e| e.path.clone()).collect();
    showcase.show(&paths, &cx()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    showcase.hide(&cx()).await.unwrap();

    let folder = exported[0]
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    drop(photos);
    assert!(!folder.exists(), "exports are deleted");
}

/// Spotlight accepts the queries built, and never returns packages: run on
/// system apps only, never the user's files.
#[tokio::test]
#[ignore = "runs Spotlight"]
async fn spotlight_searches_without_entering_packages() {
    use tsetsen_core::ports::{FileQuery, FileSearch};
    let search = tsetsen_macos::SpotlightFiles::new(vec!["/System/Applications".into()]);
    let cx = tsetsen_core::Ctx::new(tsetsen_core::RequestId::new(1));
    let query = FileQuery {
        words: vec!["textedit".into()],
        from: Some(std::time::SystemTime::UNIX_EPOCH),
        to: Some(std::time::SystemTime::now()),
        limit: 10,
    };
    let found = search.search(&query, &cx).await.unwrap();
    assert!(
        found
            .iter()
            .all(|f| !f.path.to_string_lossy().contains(".app")),
        "{found:?}"
    );
}
