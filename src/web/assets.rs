//! The page's files, embedded in the binary from a fixed list (`src/web/assets/`), served
//! at `/assets/<path>`; `/` serves `index.html`, or `locked.html` to a browser without
//! the session cookie. In debug builds only, `REDIS_LENS_DASHBOARD_ASSETS=<dir>` serves
//! the same files (the same list, nothing else) from `<dir>` instead, so the page can be
//! edited without rebuilding.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};

use super::api::error;
use super::auth::Guard;

/// The environment variable of the debug-only override.
pub const OVERRIDE_ENV: &str = "REDIS_LENS_DASHBOARD_ASSETS";

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        /// Every file the page has, by its path under `src/web/assets/`.
        pub const FILES: &[(&str, &[u8])] = &[
            $(($path, include_bytes!(concat!("assets/", $path)))),*
        ];
    };
}

assets!(
    "index.html",
    "locked.html",
    "app.css",
    "app.js",
    "api.js",
    "ui.js",
    "mock.js",
    "mark.svg",
    "views/connections.js",
    "views/overview.js",
    "views/server.js",
    "views/pubsub.js",
    "views/cmdline.js",
    "views/browse.js",
    "views/analyze.js",
    "views/streams.js",
    "views/search.js",
);

/// The content type of `path`, by its extension. `None` for a file not on the list.
pub fn content_type(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Where the files come from.
#[derive(Debug, Clone, Default)]
pub struct Assets {
    /// The debug-only override directory.
    dir: Option<PathBuf>,
}

impl Assets {
    /// The embedded files, or in a debug build the directory
    /// [`OVERRIDE_ENV`] names.
    pub fn from_env() -> Self {
        #[cfg(debug_assertions)]
        {
            let dir = std::env::var_os(OVERRIDE_ENV)
                .filter(|d| !d.is_empty())
                .map(PathBuf::from);
            Self { dir }
        }
        #[cfg(not(debug_assertions))]
        {
            Self { dir: None }
        }
    }

    #[cfg(test)]
    pub fn from_dir(dir: &Path) -> Self {
        Self {
            dir: Some(dir.to_path_buf()),
        }
    }

    /// The bytes of `path`, when it is on the list.
    async fn get(&self, path: &str) -> Option<Vec<u8>> {
        let (name, embedded) = FILES.iter().find(|(p, _)| *p == path)?;
        match &self.dir {
            // Only names on the list: never a path the request made up.
            Some(dir) => read(dir, name).await,
            None => Some(embedded.to_vec()),
        }
    }
}

async fn read(dir: &Path, name: &str) -> Option<Vec<u8>> {
    let path = dir.join(name);
    tokio::task::spawn_blocking(move || std::fs::read(path).ok())
        .await
        .ok()
        .flatten()
}

/// The file `path` as a response, or 404.
pub async fn file(assets: &Assets, path: &str) -> Response {
    let (Some(ct), Some(body)) = (content_type(path), assets.get(path).await) else {
        return error(StatusCode::NOT_FOUND, "not found");
    };
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    (h, body).into_response()
}

/// The state the page's routes need.
#[derive(Debug, Clone)]
pub struct Page {
    pub assets: Assets,
    pub guard: Guard,
}

/// `GET /assets/<path>`.
pub async fn asset(
    State(page): State<Page>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    file(&page.assets, &path).await
}

/// `GET /`: the dashboard when signed in, otherwise the page that says how to sign in.
pub async fn index(State(page): State<Page>, headers: HeaderMap) -> Response {
    let name = if page.guard.signed_in(&headers) {
        "index.html"
    } else {
        "locked.html"
    };
    file(&page.assets, name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_on_the_list_has_a_content_type() {
        assert_eq!(FILES.len(), 17);
        for (p, _) in FILES {
            assert!(content_type(p).is_some(), "{p}");
        }
        assert_eq!(
            content_type("views/overview.js"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(content_type("app.css"), Some("text/css; charset=utf-8"));
        assert_eq!(content_type("index.html"), Some("text/html; charset=utf-8"));
        assert_eq!(content_type("mark.svg"), Some("image/svg+xml"));
        assert_eq!(content_type("secrets.toml"), None);
        assert_eq!(content_type("noext"), None);
    }

    #[tokio::test]
    async fn only_listed_files_are_served_even_from_the_override_directory() {
        let dir = std::env::temp_dir().join(format!(
            "redis-lens-web-assets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("views")).unwrap();
        std::fs::write(dir.join("app.js"), b"// from disk").unwrap();
        std::fs::write(dir.join("other.js"), b"// not on the list").unwrap();
        let a = Assets::from_dir(&dir);
        assert_eq!(a.get("app.js").await.unwrap(), b"// from disk");
        assert!(a.get("other.js").await.is_none());
        assert!(a.get("../Cargo.toml").await.is_none());
        // On the list but not in the directory: nothing.
        assert!(a.get("ui.js").await.is_none());
        let embedded = Assets::default();
        assert_eq!(
            embedded.get("mark.svg").await.unwrap(),
            FILES.iter().find(|(p, _)| *p == "mark.svg").unwrap().1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
