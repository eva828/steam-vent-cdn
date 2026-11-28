//! Steam PICS product info retrieval and parsing.
//!
//! `fetch_app_info` requests the PICS product info for an app and parses the
//! KeyValues blob it contains into a list of depots that can be downloaded.

use futures::StreamExt;
use steam_vent_proto::steammessages_clientserver_appinfo::CMsgClientPICSProductInfoRequest;
use steam_vent_proto::steammessages_clientserver_appinfo::CMsgClientPICSProductInfoResponse;
use steam_vent_proto::steammessages_clientserver_appinfo::cmsg_client_picsproduct_info_request::AppInfo as PicsAppRequest;

use crate::connection::is_transient;
use crate::downloader::client::CdnDownloader;
use crate::downloader::vdf::{VdfObject, VdfValue, parse};
use crate::error::{AppError, Result, SteamSessionError, VdfError};
use steam_vent::ConnectionTrait;

/// Metadata for a single depot of an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDepot {
    pub depot_id: u32,
    /// Manifest of the `public` branch, or 0 for shared depots that have none.
    pub manifest_id: u64,
    /// Size of the public manifest as reported by PICS, or 0 when unknown.
    pub size: u64,
    pub oslist: Option<String>,
    pub language: Option<String>,
    /// Low violence builds are regional alternatives and are never downloaded.
    pub lowviolence: bool,
    pub dlc_app_id: Option<u32>,
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

    /// Whether the depot belongs to the selected language.
    ///
    /// Depots without a `language` hold content shared by every language and
    /// are always kept. `None` disables the filter.
    pub fn matches_language(&self, language: Option<&str>) -> bool {
        match (language, &self.language) {
            (None, _) | (Some(_), None) => true,
            (Some(selected), Some(list)) => list
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case(selected)),
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
    /// Depots that can be downloaded for the given platform and language.
    ///
    /// Low violence depots are regional alternatives to the regular content and
    /// are never selected; downloading both would mix two builds in one
    /// directory. The order is the order of the appinfo document, which is also
    /// the order the depots are overlaid in (later depots win).
    pub fn selected_depots(&self, os: Option<&str>, language: Option<&str>) -> Vec<&AppDepot> {
        self.depots
            .iter()
            .filter(|depot| {
                !depot.lowviolence
                    && depot.matches_language(language)
                    && os.is_none_or(|os| depot.matches_os(os))
            })
            .collect()
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

fn parse_depots(depots_object: &VdfObject, app_id: u32) -> Vec<AppDepot> {
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

        // A depot that names its own app is not shared: some DLCs self-link and
        // can only be downloaded through their own manifest.
        let depot_from_app = depot_object
            .get("depotfromapp")
            .and_then(VdfValue::as_u64)
            .map(|value| value as u32)
            .filter(|owner| *owner != app_id);

        // Newer appinfo nests the gid, older data stores it as the value itself.
        let public = depot_object.get_path(&["manifests", "public"]);
        let (manifest_id, size) = match public {
            Some(VdfValue::Object(object)) => (
                object.get("gid").and_then(VdfValue::as_u64),
                object.get("size").and_then(VdfValue::as_u64).unwrap_or(0),
            ),
            Some(scalar) => (scalar.as_u64(), 0),
            None => (None, 0),
        };
        let manifest_id = manifest_id.filter(|id| *id > 0);

        if manifest_id.is_none() && depot_from_app.is_none() {
            match public {
                Some(VdfValue::Object(_)) => tracing::warn!(
                    "Skipping depot {} of app {}: 'manifests.public' has no usable gid",
                    depot_id,
                    app_id
                ),
                Some(value) => tracing::warn!(
                    "Skipping depot {} of app {}: unrecognized 'manifests.public' value ({})",
                    depot_id,
                    app_id,
                    value.type_name()
                ),
                None => tracing::warn!(
                    "Skipping depot {} of app {}: no public manifest (not owned or not public)",
                    depot_id,
                    app_id
                ),
            }
            continue;
        }

        let config = depot_object.get("config").and_then(VdfValue::as_object);
        let oslist = config
            .and_then(|config| config.get("oslist"))
            .and_then(VdfValue::as_str)
            .map(str::to_string);
        let language = config
            .and_then(|config| config.get("language"))
            .and_then(VdfValue::as_str)
            .map(str::to_string);
        let lowviolence = config
            .and_then(|config| config.get("lowviolence"))
            .and_then(VdfValue::as_bool)
            .unwrap_or(false);
        let dlc_app_id = depot_object
            .get("dlcappid")
            .and_then(VdfValue::as_u64)
            .map(|value| value as u32);

        depots.push(AppDepot {
            depot_id,
            manifest_id: manifest_id.unwrap_or(0),
            size,
            oslist,
            language,
            lowviolence,
            dlc_app_id,
            depot_from_app,
        });
    }

    depots
}

impl CdnDownloader {
    /// Fetch and parse the PICS product info for an app.
    pub async fn fetch_app_info(&self, app_id: u32) -> Result<AppInfo> {
        tracing::debug!("Requesting PICS product info for app {}", app_id);

        let mut request = CMsgClientPICSProductInfoRequest::new();
        let mut requested_app = PicsAppRequest::new();
        requested_app.set_appid(app_id);
        request.apps.push(requested_app);
        request.set_single_response(true);

        let responses = self.fetch_pics_responses(request).await?;
        let buffer = merge_pics_responses(app_id, responses)?;
        AppInfo::parse(app_id, &buffer)
    }

    /// Send a PICS request, retrying transient network errors.
    ///
    /// `job_multi` is required: only a response with `response_pending` cleared
    /// marks the end of the reply, and `job` would stop at the first (possibly
    /// empty) message. A retry restarts the request; PICS reads are idempotent.
    async fn fetch_pics_responses(
        &self,
        request: CMsgClientPICSProductInfoRequest,
    ) -> Result<Vec<CMsgClientPICSProductInfoResponse>> {
        let config = self.steam_session.config();
        let mut attempt = 0;

        loop {
            let connection = self.steam_session.connection();
            let mut responses = Box::pin(
                connection.job_multi::<_, CMsgClientPICSProductInfoResponse>(request.clone()),
            );

            let mut collected = Vec::new();
            let error = loop {
                match responses.next().await {
                    Some(Ok(response)) => collected.push(response),
                    Some(Err(error)) => break Some(error),
                    None => break None,
                }
            };

            match error {
                None => return Ok(collected),
                Some(error)
                    if attempt < config.retry_policy.max_retries && is_transient(&error) =>
                {
                    attempt += 1;
                    tracing::debug!(
                        "Transient PICS error, retrying (attempt {}): {}",
                        attempt,
                        error
                    );
                    tokio::time::sleep(config.retry_policy.retry_delay).await;
                }
                Some(error) => return Err(SteamSessionError::from(error).into()),
            }
        }
    }
}

/// Collect the app buffer from the PICS responses of a single app request.
///
/// PICS may send an empty buffer before the final message, so the last
/// non-empty buffer wins instead of concatenating them.
fn merge_pics_responses(
    app_id: u32,
    responses: impl IntoIterator<Item = CMsgClientPICSProductInfoResponse>,
) -> Result<Vec<u8>> {
    let mut buffer: Option<Vec<u8>> = None;
    let mut missing_token = false;
    let mut found = false;
    let mut unknown = false;

    for response in responses {
        unknown |= response.unknown_appids.contains(&app_id);

        for app in response.apps {
            if app.appid() != app_id {
                continue;
            }
            found = true;
            missing_token |= app.missing_token();
            if !app.buffer().is_empty() {
                buffer = Some(app.buffer().to_vec());
            }
        }
    }

    if unknown || !found {
        return Err(AppError::UnknownAppId(app_id).into());
    }
    if missing_token {
        return Err(AppError::AccessTokenRequired(app_id).into());
    }

    buffer.ok_or_else(|| AppError::EmptyAppInfo(app_id).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto::steammessages_clientserver_appinfo::cmsg_client_picsproduct_info_response::AppInfo as PicsResponseApp;

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

    fn appinfo(body: Vec<u8>) -> Vec<u8> {
        object("appinfo", [kv_string("appid", "440"), body].concat())
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

        appinfo(
            [
                object("common", kv_string("name", "Team Fortress 2")),
                object("depots", depots_body),
            ]
            .concat(),
        )
    }

    fn depot_ids(depots: Vec<&AppDepot>) -> Vec<u32> {
        depots.iter().map(|depot| depot.depot_id).collect()
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
        assert_eq!(
            depot_ids(info.selected_depots(Some("linux"), None)),
            vec![441, 442]
        );
        assert_eq!(
            depot_ids(info.selected_depots(Some("windows"), None)),
            vec![441]
        );
        assert_eq!(depot_ids(info.selected_depots(None, None)), vec![441, 442]);
    }

    #[test]
    fn keeps_appinfo_order() {
        let info = AppInfo::parse(440, &sample()).unwrap();
        // Later depots are written after earlier ones, so the order is what
        // decides which depot wins when two of them provide the same file.
        assert_eq!(depot_ids(info.selected_depots(None, None)), vec![441, 442]);
    }

    #[test]
    fn matches_os_is_case_insensitive() {
        let depot = AppDepot {
            depot_id: 1,
            manifest_id: 1,
            size: 0,
            oslist: Some("Windows, Linux".to_string()),
            language: None,
            lowviolence: false,
            dlc_app_id: None,
            depot_from_app: None,
        };
        assert!(depot.matches_os("windows"));
        assert!(depot.matches_os("linux"));
        assert!(!depot.matches_os("macos"));
    }

    fn variant_depots() -> AppInfo {
        let mut depots_body = Vec::new();
        depots_body.extend(object(
            "branches",
            object("public", kv_string("buildid", "1")),
        ));
        // base depot without language
        depots_body.extend(object(
            "10",
            object("manifests", object("public", kv_string("gid", "100"))),
        ));
        // english depot
        depots_body.extend(object(
            "11",
            [
                object("manifests", object("public", kv_string("gid", "101"))),
                object("config", kv_string("language", "english")),
            ]
            .concat(),
        ));
        // french depot
        depots_body.extend(object(
            "12",
            [
                object("manifests", object("public", kv_string("gid", "102"))),
                object("config", kv_string("language", "french")),
            ]
            .concat(),
        ));
        // english+french depot
        depots_body.extend(object(
            "13",
            [
                object("manifests", object("public", kv_string("gid", "103"))),
                object("config", kv_string("language", "english,french")),
            ]
            .concat(),
        ));
        // low violence alternative
        depots_body.extend(object(
            "14",
            [
                object("manifests", object("public", kv_string("gid", "104"))),
                object("config", kv_string("lowviolence", "1")),
            ]
            .concat(),
        ));

        let buffer = appinfo(
            [
                object("common", kv_string("name", "App")),
                object("depots", depots_body),
            ]
            .concat(),
        );
        AppInfo::parse(440, &buffer).unwrap()
    }

    #[test]
    fn selects_language_and_skips_low_violence() {
        let info = variant_depots();
        assert_eq!(
            depot_ids(info.selected_depots(None, Some("english"))),
            vec![10, 11, 13]
        );
        assert_eq!(
            depot_ids(info.selected_depots(None, Some("french"))),
            vec![10, 12, 13]
        );
        assert_eq!(
            depot_ids(info.selected_depots(None, Some("japanese"))),
            vec![10]
        );
        assert_eq!(
            depot_ids(info.selected_depots(None, None)),
            vec![10, 11, 12, 13]
        );
    }

    #[test]
    fn parses_legacy_scalar_public_manifest() {
        let buffer = appinfo(
            [
                object("common", kv_string("name", "Old App")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "7"))),
                        object("441", object("manifests", kv_string("public", "12345"))),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );

        let info = AppInfo::parse(440, &buffer).unwrap();
        assert_eq!(info.depots.len(), 1);
        assert_eq!(info.depots[0].manifest_id, 12345);
        assert_eq!(info.depots[0].size, 0);
    }

    #[test]
    fn skips_depots_with_zero_gid() {
        let mut depots_body = Vec::new();
        depots_body.extend(object(
            "branches",
            object("public", kv_string("buildid", "100")),
        ));
        depots_body.extend(object(
            "700",
            object("manifests", object("public", kv_string("gid", "0"))),
        ));

        let buffer = appinfo(
            [
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
    fn skips_depots_without_public_manifest() {
        let mut depots_body = Vec::new();
        depots_body.extend(object(
            "branches",
            object("public", kv_string("buildid", "100")),
        ));
        depots_body.extend(object("700", object("manifests", vec![])));

        let buffer = appinfo(
            [
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
        let buffer = appinfo(
            [
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
        let buffer = appinfo(
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
        let buffer = appinfo(
            [
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
        assert_eq!(info.depots[0].depot_from_app, Some(228980));
        assert_eq!(info.depots[0].manifest_id, 5);
    }

    #[test]
    fn parses_shared_depot_without_manifests() {
        let buffer = appinfo(
            [
                object("common", kv_string("name", "App")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object("200", kv_string("depotfromapp", "228980")),
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

    #[test]
    fn self_linked_depot_is_not_shared() {
        let buffer = appinfo(
            [
                object("common", kv_string("name", "App")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object(
                            "441",
                            [
                                object("manifests", object("public", kv_string("gid", "5"))),
                                kv_string("depotfromapp", "440"),
                            ]
                            .concat(),
                        ),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );

        let info = AppInfo::parse(440, &buffer).unwrap();
        assert_eq!(info.depots.len(), 1);
        assert_eq!(info.depots[0].depot_from_app, None);
        assert_eq!(info.depots[0].manifest_id, 5);
    }

    #[test]
    fn skips_self_linked_depot_without_manifest() {
        let buffer = appinfo(
            [
                object("common", kv_string("name", "App")),
                object(
                    "depots",
                    [
                        object("branches", object("public", kv_string("buildid", "1"))),
                        object("441", kv_string("depotfromapp", "440")),
                    ]
                    .concat(),
                ),
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

    fn pics_response(
        app_id: u32,
        buffer: &[u8],
        missing_token: bool,
    ) -> CMsgClientPICSProductInfoResponse {
        let mut app = PicsResponseApp::new();
        app.set_appid(app_id);
        app.set_buffer(buffer.to_vec());
        app.set_missing_token(missing_token);

        let mut response = CMsgClientPICSProductInfoResponse::new();
        response.apps.push(app);
        response
    }

    #[test]
    fn merge_keeps_last_non_empty_buffer() {
        let empty = pics_response(440, b"", false);
        let full = pics_response(440, b"appinfo", false);

        assert_eq!(
            merge_pics_responses(440, [empty, full]).unwrap(),
            b"appinfo"
        );
    }

    #[test]
    fn merge_reports_missing_token() {
        let response = pics_response(440, b"", true);
        assert!(matches!(
            merge_pics_responses(440, [response]),
            Err(crate::error::SteamVentError::AppInfo(
                AppError::AccessTokenRequired(440)
            ))
        ));
    }

    #[test]
    fn merge_reports_unknown_app() {
        let mut response = CMsgClientPICSProductInfoResponse::new();
        response.unknown_appids.push(440);

        assert!(matches!(
            merge_pics_responses(440, [response]),
            Err(crate::error::SteamVentError::AppInfo(
                AppError::UnknownAppId(440)
            ))
        ));
    }

    #[test]
    fn merge_reports_empty_response() {
        let response = pics_response(440, b"", false);
        assert!(matches!(
            merge_pics_responses(440, [response]),
            Err(crate::error::SteamVentError::AppInfo(
                AppError::EmptyAppInfo(440)
            ))
        ));
    }

    #[test]
    fn merge_reports_missing_app_entry() {
        let other = pics_response(441, b"appinfo", false);
        assert!(matches!(
            merge_pics_responses(440, [other]),
            Err(crate::error::SteamVentError::AppInfo(
                AppError::UnknownAppId(440)
            ))
        ));
    }
}
