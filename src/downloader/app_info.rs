//! Steam PICS product info retrieval and parsing.
//!
//! `fetch_app_info` requests the PICS product info for an app and parses the
//! binary KeyValues blob it contains into a list of depots that can be downloaded.

use futures::StreamExt;
use steam_vent_proto::steammessages_clientserver_appinfo::CMsgClientPICSProductInfoRequest;
use steam_vent_proto::steammessages_clientserver_appinfo::CMsgClientPICSProductInfoResponse;
use steam_vent_proto::steammessages_clientserver_appinfo::cmsg_client_picsproduct_info_request::AppInfo as PicsAppRequest;

use crate::downloader::client::CdnDownloader;
use crate::downloader::vdf::{VdfValue, parse};
use crate::error::{AppError, Result, SteamSessionError, VdfError};
use steam_vent::ConnectionTrait;

/// Metadata for a single depot of an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub size: u64,
    pub oslist: Option<String>,
    pub dlc_app_id: Option<u32>,
    pub shared_install: bool,
    /// For shared installs, the app that actually owns and provides this depot.
    pub depot_from_app: Option<u32>,
}

impl AppDepot {
    /// Whether the depot is usable on the given platform name (as used in `oslist`).
    ///
    /// Depots without an `oslist` are considered usable everywhere.
    /// Comparison is ASCII case-insensitive because real-world `oslist`
    /// values vary in case.
    pub fn matches_os(&self, os: &str) -> bool {
        match &self.oslist {
            Some(list) => list
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case(os)),
            None => true,
        }
    }
}

/// Product info for an app, limited to what is needed for downloading depots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub app_id: u32,
    pub name: String,
    pub build_id: u64,
    /// Unix timestamp of the public branch build, when available.
    pub build_time: Option<u64>,
    /// Install directory name below `steamapps/common`.
    pub installdir: Option<String>,
    pub depots: Vec<AppDepot>,
}

impl AppInfo {
    /// Return the depots matching the given platform, or all depots when `os` is `None`.
    pub fn depots_for_os(&self, os: Option<&str>) -> Vec<&AppDepot> {
        match os {
            Some(os) => self
                .depots
                .iter()
                .filter(|depot| depot.matches_os(os))
                .collect(),
            None => self.depots.iter().collect(),
        }
    }

    fn parse(app_id: u32, buffer: &[u8]) -> Result<Self> {
        let document = parse(buffer)?;
        let root = document.value.as_object().ok_or(VdfError::ExpectedObject {
            found: document.value.type_name(),
        })?;

        let name = root
            .get_path(&["common", "name"])
            .and_then(VdfValue::as_str)
            .ok_or(VdfError::MissingKey {
                path: "common.name",
            })?
            .to_string();

        let build_id = root
            .get_path(&["depots", "branches", "public", "buildid"])
            .and_then(VdfValue::as_u64)
            .ok_or(AppError::NoPublicBuild(app_id))?;
        let build_time = root
            .get_path(&["depots", "branches", "public", "timeupdated"])
            .and_then(VdfValue::as_u64);
        let installdir = root
            .get_path(&["config", "installdir"])
            .and_then(VdfValue::as_str)
            .map(str::to_string);

        let depots_object = root
            .get("depots")
            .and_then(VdfValue::as_object)
            .ok_or(VdfError::MissingKey { path: "depots" })?;

        let depots = parse_depots(depots_object, app_id);

        if depots.is_empty() {
            return Err(AppError::NoDepots(app_id).into());
        }

        Ok(AppInfo {
            app_id,
            name,
            build_id,
            build_time,
            installdir,
            depots,
        })
    }
}

fn parse_depots(depots_object: &crate::downloader::vdf::VdfObject, app_id: u32) -> Vec<AppDepot> {
    let mut depots = Vec::new();

    for (key, value) in depots_object.iter() {
        if key == "branches" {
            continue;
        }

        let depot_id: u32 = match key.parse() {
            Ok(id) => id,
            Err(_) => {
                tracing::debug!(
                    "Skipping non-depot key '{}' in depots of app {}",
                    key,
                    app_id
                );
                continue;
            }
        };

        let Some(depot_object) = value.as_object() else {
            tracing::warn!(
                "Skipping depot {} of app {}: expected object, found {}",
                depot_id,
                app_id,
                value.type_name()
            );
            continue;
        };

        let shared_install = depot_object
            .get("sharedinstall")
            .and_then(VdfValue::as_bool)
            .unwrap_or(false);
        let depot_from_app = depot_object
            .get("depotfromapp")
            .and_then(VdfValue::as_u64)
            .map(|value| value as u32);

        let public = depot_object
            .get_path(&["manifests", "public"])
            .and_then(VdfValue::as_object);
        let manifest_id = public
            .and_then(|public| public.get("gid"))
            .and_then(VdfValue::as_u64);
        let size = public
            .and_then(|public| public.get("size"))
            .and_then(VdfValue::as_u64)
            .unwrap_or(0);

        if depot_from_app.is_none() && manifest_id.is_none() {
            tracing::warn!(
                "Skipping depot {} of app {}: no public manifest (not owned or not public)",
                depot_id,
                app_id
            );
            continue;
        }

        let oslist = depot_object
            .get_path(&["config", "oslist"])
            .and_then(VdfValue::as_str)
            .map(str::to_string);
        let dlc_app_id = depot_object
            .get("dlcappid")
            .and_then(VdfValue::as_u64)
            .map(|value| value as u32);

        depots.push(AppDepot {
            depot_id,
            manifest_id: manifest_id.unwrap_or(0),
            size,
            oslist,
            dlc_app_id,
            shared_install,
            depot_from_app,
        });
    }

    depots
}

impl CdnDownloader {
    /// Fetch and parse the PICS product info for an app.
    ///
    /// A single app id is requested with `single_response`, so the server
    /// normally answers with one response. `job_multi` is still used because
    /// the final response is marked via `response_pending`; each app buffer
    /// is atomic, so the last non-empty buffer wins instead of concatenating.
    pub async fn fetch_app_info(&self, app_id: u32) -> Result<AppInfo> {
        tracing::debug!("Requesting PICS product info for app {}", app_id);

        let mut request = CMsgClientPICSProductInfoRequest::new();
        let mut requested_app = PicsAppRequest::new();
        requested_app.set_appid(app_id);
        request.apps.push(requested_app);
        request.set_single_response(true);

        let connection = self.steam_session.connection();
        let mut responses =
            Box::pin(connection.job_multi::<_, CMsgClientPICSProductInfoResponse>(request));

        let mut buffer: Option<Vec<u8>> = None;
        let mut missing_token = false;
        let mut found = false;
        let mut unknown = false;

        while let Some(response) = responses.next().await {
            let response = response.map_err(SteamSessionError::from)?;

            unknown |= response.unknown_appids.contains(&app_id);

            for app in response.apps {
                if app.appid() == app_id {
                    found = true;
                    missing_token |= app.missing_token();
                    if !app.buffer().is_empty() {
                        buffer = Some(app.buffer().to_vec());
                    }
                }
            }
        }

        if unknown || !found {
            return Err(AppError::UnknownAppId(app_id).into());
        }
        if missing_token {
            return Err(AppError::AccessTokenRequired(app_id).into());
        }

        let buffer = buffer.ok_or(AppError::EmptyAppInfo(app_id))?;
        AppInfo::parse(app_id, &buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPE_OBJECT: u8 = 0x00;
    const TYPE_STRING: u8 = 0x01;
    const TYPE_END: u8 = 0x08;

    fn cstr(s: &str) -> Vec<u8> {
        let mut out = s.as_bytes().to_vec();
        out.push(0);
        out
    }

    fn kv_string(key: &str, value: &str) -> Vec<u8> {
        let mut out = vec![TYPE_STRING];
        out.extend(cstr(key));
        out.extend(cstr(value));
        out
    }

    fn object(key: &str, body: Vec<u8>) -> Vec<u8> {
        let mut out = vec![TYPE_OBJECT];
        out.extend(cstr(key));
        out.extend(body);
        out.push(TYPE_END);
        out
    }

    fn sample() -> Vec<u8> {
        let mut depots_body = Vec::new();
        // branches { public { buildid "100" } }
        depots_body.extend(object(
            "branches",
            object("public", kv_string("buildid", "100")),
        ));
        // depot 441 (all platforms)
        depots_body.extend(object(
            "441",
            object("manifests", object("public", kv_string("gid", "555"))),
        ));
        // depot 442 (linux only)
        let mut depot_442 = object("manifests", object("public", kv_string("gid", "556")));
        depot_442.extend(object("config", kv_string("oslist", "linux")));
        depots_body.extend(object("442", depot_442));

        object(
            "appinfo",
            [
                kv_string("appid", "440"),
                object("common", kv_string("name", "Team Fortress 2")),
                object("depots", depots_body),
            ]
            .concat(),
        )
    }

    #[test]
    fn parses_app_info() {
        let info = AppInfo::parse(440, &sample()).unwrap();
        assert_eq!(info.app_id, 440);
        assert_eq!(info.name, "Team Fortress 2");
        assert_eq!(info.build_id, 100);
        assert_eq!(info.depots.len(), 2);
        assert_eq!(info.depots[0].depot_id, 441);
        assert_eq!(info.depots[0].manifest_id, 555);
        assert_eq!(info.depots[1].depot_id, 442);
        assert_eq!(info.depots[1].oslist.as_deref(), Some("linux"));
    }

    #[test]
    fn filters_depots_by_os() {
        let info = AppInfo::parse(440, &sample()).unwrap();
        let linux: Vec<u32> = info
            .depots_for_os(Some("linux"))
            .iter()
            .map(|d| d.depot_id)
            .collect();
        assert_eq!(linux, vec![441, 442]);

        let windows: Vec<u32> = info
            .depots_for_os(Some("windows"))
            .iter()
            .map(|d| d.depot_id)
            .collect();
        assert_eq!(windows, vec![441]);

        assert_eq!(info.depots_for_os(None).len(), 2);
    }

    #[test]
    fn matches_os_is_case_insensitive() {
        let depot = AppDepot {
            depot_id: 1,
            manifest_id: 1,
            size: 0,
            oslist: Some("Windows, Linux".to_string()),
            dlc_app_id: None,
            shared_install: false,
            depot_from_app: None,
        };
        assert!(depot.matches_os("windows"));
        assert!(depot.matches_os("linux"));
        assert!(!depot.matches_os("macos"));
    }

    #[test]
    fn skips_depots_without_public_manifest() {
        let mut depots_body = Vec::new();
        depots_body.extend(object(
            "branches",
            object("public", kv_string("buildid", "100")),
        ));
        depots_body.extend(object("700", object("manifests", vec![])));

        let buffer = object(
            "appinfo",
            [
                kv_string("appid", "440"),
                object("common", kv_string("name", "App")),
                object("depots", depots_body),
            ]
            .concat(),
        );

        assert!(matches!(
            AppInfo::parse(440, &buffer),
            Err(crate::error::SteamVentError::AppInfo(AppError::NoDepots(
                440
            )))
        ));
    }

    #[test]
    fn rejects_missing_build_id() {
        let buffer = object(
            "appinfo",
            [
                kv_string("appid", "440"),
                object("common", kv_string("name", "App")),
                object(
                    "depots",
                    object(
                        "441",
                        object("manifests", object("public", kv_string("gid", "1"))),
                    ),
                ),
            ]
            .concat(),
        );
        assert!(matches!(
            AppInfo::parse(440, &buffer),
            Err(crate::error::SteamVentError::AppInfo(
                AppError::NoPublicBuild(440)
            ))
        ));
    }

    #[test]
    fn rejects_missing_name() {
        let buffer = object(
            "appinfo",
            [
                object("common", vec![]),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object(
                            "441",
                            object("manifests", object("public", kv_string("gid", "2"))),
                        ),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        assert!(matches!(
            AppInfo::parse(440, &buffer),
            Err(crate::error::SteamVentError::Vdf(VdfError::MissingKey {
                path: "common.name"
            }))
        ));
    }

    #[test]
    fn parses_install_dir_and_shared_depot() {
        let buffer = object(
            "appinfo",
            [
                kv_string("appid", "1"),
                object("common", kv_string("name", "App")),
                object("config", kv_string("installdir", "AppDir")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object(
                            "100",
                            [
                                object("manifests", object("public", kv_string("gid", "5"))),
                                kv_string("sharedinstall", "1"),
                                kv_string("depotfromapp", "228980"),
                            ]
                            .concat(),
                        ),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );

        let info = AppInfo::parse(1, &buffer).unwrap();
        assert_eq!(info.installdir.as_deref(), Some("AppDir"));
        assert_eq!(info.build_time, None);
        assert_eq!(info.depots.len(), 1);
        assert!(info.depots[0].shared_install);
        assert_eq!(info.depots[0].depot_from_app, Some(228980));
        assert_eq!(info.depots[0].manifest_id, 5);
    }

    #[test]
    fn parses_shared_depot_without_manifests() {
        let buffer = object(
            "appinfo",
            [
                kv_string("appid", "1"),
                object("common", kv_string("name", "App")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object(
                            "200",
                            [
                                kv_string("sharedinstall", "1"),
                                kv_string("depotfromapp", "228980"),
                            ]
                            .concat(),
                        ),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );

        let info = AppInfo::parse(1, &buffer).unwrap();
        assert_eq!(info.depots.len(), 1);
        assert_eq!(info.depots[0].depot_id, 200);
        assert_eq!(info.depots[0].manifest_id, 0);
        assert_eq!(info.depots[0].depot_from_app, Some(228980));
    }
}
