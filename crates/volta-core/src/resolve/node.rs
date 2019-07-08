use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use super::{registry_fetch_error, serial};
use crate::distro::node::NodeDistro;
use crate::error::ErrorDetails;
use crate::fs::{ensure_containing_dir_exists, read_file_opt};
use crate::hook::ToolHooks;
use crate::path;
use crate::style::progress_spinner;
use crate::version::VersionSpec;
use cfg_if::cfg_if;
use headers_011::Headers011;
use log::debug;
use reqwest;
use reqwest::hyper_011::header::{CacheControl, CacheDirective, Expires, HttpDate};
use semver::{Version, VersionReq};
use tempfile::NamedTempFile;
use volta_fail::{throw, Fallible, ResultExt};

// ISSUE (#86): Move public repository URLs to config file
cfg_if! {
    if #[cfg(feature = "mock-network")] {
        fn public_node_version_index() -> String {
            format!("{}/node-dist/index.json", mockito::SERVER_URL)
        }
    } else {
        /// Returns the URL of the index of available Node versions on the public Node server.
        fn public_node_version_index() -> String {
            "https://nodejs.org/dist/index.json".to_string()
        }
    }
}

pub fn resolve(matching: VersionSpec, hooks: Option<&ToolHooks<NodeDistro>>) -> Fallible<Version> {
    match matching {
        VersionSpec::Latest => resolve_latest(hooks),
        VersionSpec::Lts => resolve_lts(hooks),
        VersionSpec::Semver(requirement) => resolve_semver(requirement, hooks),
        VersionSpec::Exact(version) => Ok(version),
    }
}

fn resolve_latest(hooks: Option<&ToolHooks<NodeDistro>>) -> Fallible<Version> {
    // NOTE: This assumes the registry always produces a list in sorted order
    //       from newest to oldest. This should be specified as a requirement
    //       when we document the plugin API.
    let url = match hooks {
        Some(&ToolHooks {
            latest: Some(ref hook),
            ..
        }) => {
            debug!("Using node.latest hook to determine node index URL");
            hook.resolve("index.json")?
        }
        _ => public_node_version_index(),
    };
    let version_opt = match_node_version(&url, |_| true)?;

    if let Some(version) = version_opt {
        debug!("Found latest node version ({}) from {}", version, url);
        Ok(version)
    } else {
        throw!(ErrorDetails::NodeVersionNotFound {
            matching: "latest".to_string()
        })
    }
}

fn resolve_lts(_hooks: Option<&ToolHooks<NodeDistro>>) -> Fallible<Version> {
    VersionSpec::parse_version("1.0.0")
}

fn resolve_semver(
    _requirement: VersionReq,
    _hooks: Option<&ToolHooks<NodeDistro>>,
) -> Fallible<Version> {
    VersionSpec::parse_version("1.0.0")
}

fn match_node_version(
    url: &str,
    predicate: impl Fn(&NodeEntry) -> bool,
) -> Fallible<Option<Version>> {
    let index = resolve_node_versions(url)?.into_index()?;
    let mut entries = index.entries.into_iter();
    Ok(entries
        .find(predicate)
        .map(|NodeEntry { version, .. }| version))
}

/// The index of the public Node server.
pub struct NodeIndex {
    pub(super) entries: Vec<NodeEntry>,
}

#[derive(Debug)]
pub struct NodeEntry {
    pub version: Version,
    pub npm: Version,
    pub files: NodeDistroFiles,
    pub lts: bool,
}

/// The set of available files on the public Node server for a given Node version.
#[derive(Debug)]
pub struct NodeDistroFiles {
    pub files: HashSet<String>,
}

/// Reads a public index from the Node cache, if it exists and hasn't expired.
fn read_cached_opt() -> Fallible<Option<serial::RawNodeIndex>> {
    let expiry_file = path::node_index_expiry_file()?;
    let expiry = read_file_opt(&expiry_file)
        .with_context(|_| ErrorDetails::ReadNodeIndexExpiryError { file: expiry_file })?;

    if let Some(string) = expiry {
        let expiry_date = HttpDate::from_str(&string)
            .with_context(|_| ErrorDetails::ParseNodeIndexExpiryError)?;
        let current_date = HttpDate::from(SystemTime::now());

        if current_date < expiry_date {
            let index_file = path::node_index_file()?;
            let cached = read_file_opt(&index_file)
                .with_context(|_| ErrorDetails::ReadNodeIndexCacheError { file: index_file })?;

            if let Some(string) = cached {
                return serde_json::de::from_str(&string)
                    .with_context(|_| ErrorDetails::ParseNodeIndexCacheError);
            }
        }
    }

    Ok(None)
}

/// Get the cache max-age of an HTTP reponse.
fn max_age(response: &reqwest::Response) -> u32 {
    if let Some(cache_control_header) = response.headers().get_011::<CacheControl>() {
        for cache_directive in cache_control_header.iter() {
            if let CacheDirective::MaxAge(max_age) = cache_directive {
                return *max_age;
            }
        }
    }

    // Default to four hours.
    4 * 60 * 60
}

fn resolve_node_versions(url: &str) -> Fallible<serial::RawNodeIndex> {
    match read_cached_opt()? {
        Some(serial) => Ok(serial),
        None => {
            let spinner = progress_spinner(&format!("Fetching public registry: {}", url));

            let mut response: reqwest::Response =
                reqwest::get(url).with_context(registry_fetch_error("Node", url))?;
            let response_text = response
                .text()
                .with_context(registry_fetch_error("Node", url))?;
            let index: serial::RawNodeIndex = serde_json::de::from_str(&response_text)
                .with_context(|_| ErrorDetails::ParseNodeIndexError {
                    from_url: url.to_string(),
                })?;

            let tmp_root = path::tmp_dir()?;
            // Helper to lazily determine temp dir string, without moving the file into the closures below
            let get_tmp_root = || tmp_root.to_owned();

            let cached = NamedTempFile::new_in(&tmp_root).with_context(|_| {
                ErrorDetails::CreateTempFileError {
                    in_dir: get_tmp_root(),
                }
            })?;

            // Block to borrow cached for cached_file.
            {
                let mut cached_file: &File = cached.as_file();
                cached_file
                    .write(response_text.as_bytes())
                    .with_context(|_| ErrorDetails::WriteNodeIndexCacheError {
                        file: cached.path().to_path_buf(),
                    })?;
            }

            let index_cache_file = path::node_index_file()?;
            ensure_containing_dir_exists(&index_cache_file)?;
            cached.persist(&index_cache_file).with_context(|_| {
                ErrorDetails::WriteNodeIndexCacheError {
                    file: index_cache_file,
                }
            })?;

            let expiry = NamedTempFile::new_in(&tmp_root).with_context(|_| {
                ErrorDetails::CreateTempFileError {
                    in_dir: get_tmp_root(),
                }
            })?;

            // Block to borrow expiry for expiry_file.
            {
                let mut expiry_file: &File = expiry.as_file();

                let result = if let Some(expires_header) = response.headers().get_011::<Expires>() {
                    write!(expiry_file, "{}", expires_header)
                } else {
                    let expiry_date =
                        SystemTime::now() + Duration::from_secs(max_age(&response).into());

                    write!(expiry_file, "{}", HttpDate::from(expiry_date))
                };

                result.with_context(|_| ErrorDetails::WriteNodeIndexExpiryError {
                    file: expiry.path().to_path_buf(),
                })?;
            }

            let index_expiry_file = path::node_index_expiry_file()?;
            ensure_containing_dir_exists(&index_expiry_file)?;
            expiry.persist(&index_expiry_file).with_context(|_| {
                ErrorDetails::WriteNodeIndexExpiryError {
                    file: index_expiry_file,
                }
            })?;

            spinner.finish_and_clear();
            Ok(index)
        }
    }
}
