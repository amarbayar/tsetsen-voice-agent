//! What this OS offers, and which adapter provides each service.
//!
//! The tools are the same on every OS; only the adapters below differ.
//! Supporting a service on another OS means filling in its slot here, and
//! every tool that needs it appears there on its own.

use std::path::PathBuf;
use std::sync::Arc;

use tsetsen_core::Os;
use tsetsen_core::ports::{FileSearch, Launcher, PhotoLibrary, Showcase};

/// The services that come from the OS itself.
#[derive(Default)]
pub(crate) struct Local {
    pub(crate) photos: Option<Arc<dyn PhotoLibrary>>,
    pub(crate) showcase: Option<Arc<dyn Showcase>>,
    pub(crate) files: Option<Arc<dyn FileSearch>>,
    pub(crate) launcher: Option<Arc<dyn Launcher>>,
}

/// What the user turned off.
pub(crate) struct Choices {
    pub(crate) photos: bool,
    pub(crate) files: bool,
    /// The photos helper, if not the one built with tsetsen.
    pub(crate) photos_helper: Option<PathBuf>,
}

/// The folders where people keep their own files.
fn user_folders() -> Vec<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    home.map(|h| tsetsen_files::user_folders(&PathBuf::from(h)))
        .unwrap_or_default()
}

/// Connects this OS's services, saying which are on and why others are not.
pub(crate) async fn connect(choices: &Choices) -> Local {
    let mut local = Local {
        launcher: Some(Arc::new(tsetsen_files::SystemLauncher)),
        ..Local::default()
    };
    if choices.files {
        let folders = user_folders();
        if folders.is_empty() {
            crate::warn("files are off: no Documents, Desktop or Downloads folder");
        } else {
            local.files = Some(file_search(folders));
        }
    }
    if choices.photos {
        connect_photos(choices, &mut local).await;
    }
    crate::note(&format!(
        "{}: {}",
        Os::current().name(),
        offered(&local).join(", ")
    ));
    local
}

fn offered(local: &Local) -> Vec<&'static str> {
    [
        ("pictures", local.photos.is_some()),
        ("showing", local.showcase.is_some()),
        ("files", local.files.is_some()),
        ("opening", local.launcher.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect()
}

/// Spotlight on macOS: fast, and it reads the text inside files.
#[cfg(target_os = "macos")]
fn file_search(folders: Vec<PathBuf>) -> Arc<dyn FileSearch> {
    Arc::new(tsetsen_macos::SpotlightFiles::new(folders))
}

/// Elsewhere, for now, a walk over the folders by name. Windows Search and
/// Tracker/Baloo adapters can take this slot.
#[cfg(not(target_os = "macos"))]
fn file_search(folders: Vec<PathBuf>) -> Arc<dyn FileSearch> {
    Arc::new(tsetsen_files::WalkFiles::new(folders))
}

#[cfg(target_os = "macos")]
async fn connect_photos(choices: &Choices, local: &mut Local) {
    use std::time::Duration;
    use tsetsen_core::{Ctx, RequestId};
    use tsetsen_macos::{BUILT_HELPER, MacPhotos, MacShowcase};

    let helper = choices
        .photos_helper
        .clone()
        .unwrap_or_else(|| PathBuf::from(BUILT_HELPER));
    let photos = MacPhotos::new(&helper);
    let cx = Ctx::new(RequestId::new(0)).with_timeout(Duration::from_secs(10));
    match photos.status(&cx).await {
        Ok(status) if matches!(status.as_str(), "authorized" | "limited" | "not_determined") => {
            local.photos = Some(Arc::new(photos));
            local.showcase = Some(Arc::new(MacShowcase::new(&helper, "Tsetsen")));
        }
        Ok(status) => crate::warn(&format!(
            "pictures are off: Photos access is {status} (System Settings → Privacy & Security → Photos)"
        )),
        Err(error) => crate::warn(&format!("pictures are off: {error}")),
    }
}

/// No photo library adapter yet on this OS.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)] // the same signature as the macOS version
async fn connect_photos(_: &Choices, _: &mut Local) {
    crate::note(&format!(
        "pictures are not supported on {} yet",
        Os::current().name()
    ));
}
