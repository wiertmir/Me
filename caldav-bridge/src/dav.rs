use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};

use crate::{
    AppState, DavError,
    auth::Signed,
    backend::Collection,
    path::{self, Kind, Target},
    xml::{self, Prop},
};

const ALL: [Prop; 10] = [
    Prop::ResourceType,
    Prop::DisplayName,
    Prop::CurrentUserPrincipal,
    Prop::CalendarHomeSet,
    Prop::SupportedComponents,
    Prop::CTag,
    Prop::Color,
    Prop::SupportedReports,
    Prop::Privileges,
    Prop::Owner,
];

pub async fn handle(
    State(state): State<AppState>,
    signed: Signed,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, DavError> {
    match method.as_str() {
        "OPTIONS" => Ok((
            [
                ("dav", "1, calendar-access"),
                ("allow", "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE"),
            ],
            StatusCode::OK,
        )
            .into_response()),
        "PROPFIND" => propfind(&state, &signed, &uri, &headers, &body).await,
        _ => Err(DavError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not supported",
        )),
    }
}

fn not_found() -> DavError {
    DavError::new(StatusCode::NOT_FOUND, "")
}

async fn propfind(
    state: &AppState,
    signed: &Signed,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, DavError> {
    let target = path::parse(uri.path(), &signed.username).ok_or_else(not_found)?;
    let depth = match headers.get("depth").and_then(|v| v.to_str().ok()) {
        None | Some("0") => 0,
        Some("1") => 1,
        Some("infinity") => {
            return Err(
                DavError::new(StatusCode::FORBIDDEN, "depth infinity is not supported")
                    .precondition("propfind-finite-depth"),
            );
        }
        Some(_) => return Err(DavError::new(StatusCode::BAD_REQUEST, "bad Depth")),
    };
    let body = std::str::from_utf8(body)
        .map_err(|_| DavError::new(StatusCode::BAD_REQUEST, "body is not UTF-8"))?;
    let asked = xml::read_propfind(body)?.props;

    let mut entries: Vec<(Target, Option<Collection>)> = Vec::new();
    match &target {
        Target::Root | Target::Principal => entries.push((target.clone(), None)),
        Target::Home => {
            entries.push((target.clone(), None));
            if depth == 1 {
                for c in state.backend(signed.user_id).collections().await? {
                    entries.push((Target::Collection(c.kind, c.id), Some(c)));
                }
            }
        }
        Target::Collection(kind, id) => {
            let c = state
                .backend(signed.user_id)
                .collections()
                .await?
                .into_iter()
                .find(|c| c.kind == *kind && c.id == *id)
                .ok_or_else(not_found)?;
            entries.push((target.clone(), Some(c)));
        }
        Target::Item(..) => return Err(not_found()),
    }

    let responses: Vec<_> = entries
        .iter()
        .map(|(t, c)| {
            let values = |props: &[Prop]| -> (Vec<_>, Vec<_>) {
                let mut found = Vec::new();
                let mut missing = Vec::new();
                for p in props {
                    match value(t, c.as_ref(), &signed.username, p) {
                        Some(v) => found.push((p.clone(), v)),
                        None => missing.push(p.clone()),
                    }
                }
                (found, missing)
            };
            let (found, missing) = match &asked {
                Some(props) => values(props),
                // "All" is only what exists for this target.
                None => (values(&ALL).0, Vec::new()),
            };
            xml::Response {
                href: path::href(t, &signed.username),
                found,
                missing,
            }
        })
        .collect();
    Ok((
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml::multistatus(&responses),
    )
        .into_response())
}

/// The inner XML of `prop` for a target, or `None` when it has no such property.
fn value(target: &Target, c: Option<&Collection>, username: &str, prop: &Prop) -> Option<String> {
    let href = |t: Target| {
        format!(
            "<D:href>{}</D:href>",
            xml::escape(&path::href(&t, username))
        )
    };
    let principal = href(Target::Principal);
    Some(match (prop, target, c) {
        (Prop::ResourceType, Target::Principal, _) => "<D:collection/><D:principal/>".into(),
        (Prop::ResourceType, Target::Collection(..), _) => "<D:collection/><C:calendar/>".into(),
        (Prop::ResourceType, _, _) => "<D:collection/>".into(),
        (Prop::DisplayName, _, Some(c)) => xml::escape(&c.name),
        (Prop::DisplayName, _, None) => xml::escape(username),
        (Prop::CurrentUserPrincipal, _, _) => principal,
        (Prop::CalendarHomeSet, Target::Principal, _) => href(Target::Home),
        (Prop::SupportedComponents, _, Some(c)) => match c.kind {
            Kind::Events => "<C:comp name=\"VEVENT\"/>".into(),
            Kind::Todos => "<C:comp name=\"VTODO\"/>".into(),
        },
        (Prop::CTag, _, Some(c)) => c.sync_token.to_string(),
        (Prop::Color, _, Some(c)) => xml::escape(&c.color),
        (Prop::SupportedReports, _, Some(_)) => ["calendar-multiget", "calendar-query"]
            .map(|r| {
                format!("<D:supported-report><D:report><C:{r}/></D:report></D:supported-report>")
            })
            .concat(),
        (Prop::Privileges, _, Some(_)) => ["read", "write", "write-content", "bind", "unbind"]
            .map(|p| format!("<D:privilege><D:{p}/></D:privilege>"))
            .concat(),
        (Prop::Owner, _, Some(_)) => principal,
        _ => return None,
    })
}
