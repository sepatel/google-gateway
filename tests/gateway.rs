use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use google_gateway::app::{AppState, router};
use google_gateway::config::AppConfig;
use google_gateway::google::Endpoints;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{body_json, body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "opencode-token-0123456789abcdef0123";

struct Harness {
    app: Router,
    google: MockServer,
    _data: tempfile::TempDir,
}

async fn harness() -> Harness {
    let google = MockServer::start().await;
    let data = tempfile::tempdir().unwrap();
    let data_dir = data.path().to_string_lossy().into_owned();
    let config = AppConfig::from_lookup(|name| {
        Some(
            match name {
                "GOOGLE_GATEWAY_GOOGLE_CLIENT_ID" => "client-id",
                "GOOGLE_GATEWAY_GOOGLE_CLIENT_SECRET" => "client-secret",
                "GOOGLE_GATEWAY_PUBLIC_URL" => "https://mail.example.com",
                "GOOGLE_GATEWAY_CLIENTS" => "opencode=opencode-token-0123456789abcdef0123",
                "GOOGLE_GATEWAY_ENCRYPTION_KEY" => "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
                "GOOGLE_GATEWAY_DATA_DIR" => data_dir.as_str(),
                _ => return None,
            }
            .to_string(),
        )
    })
    .unwrap();
    let endpoints = Endpoints {
        authorize: format!("{}/authorize", google.uri()),
        token: format!("{}/token", google.uri()),
        gmail: format!("{}/gmail/v1", google.uri()),
        calendar: format!("{}/calendar/v3", google.uri()),
        people: format!("{}/people/v1", google.uri()),
    };
    let state: Arc<AppState> = AppState::new(config, endpoints).await.unwrap();
    Harness {
        app: router(state),
        google,
        _data: data,
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn authed(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn rpc(app: &Router, endpoint: &str, method: &str, params: Value) -> Value {
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let (status, body) = send(app, authed("POST", endpoint, Some(request))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()
}

async fn call_tool(app: &Router, name: &str, arguments: Value) -> Value {
    rpc(
        app,
        "/mcp",
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await["result"]
        .clone()
}

const ALL_SCOPES: &str = "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/contacts.readonly https://www.googleapis.com/auth/contacts.other.readonly https://www.googleapis.com/auth/directory.readonly";

fn refresh_succeeds() -> Mock {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-2", "expires_in": 3600,
        })))
}

async fn link(h: &Harness, email: &str) {
    link_granting(h, email, ALL_SCOPES).await;
}

async fn link_granting(h: &Harness, email: &str, scopes: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "scope": scopes,
        })))
        .up_to_n_times(1)
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "emailAddress": email })))
        .up_to_n_times(1)
        .mount(&h.google)
        .await;

    let (status, body) = send(&h.app, authed("POST", "/accounts/link", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(
        serde_json::from_str::<Value>(&body).unwrap()["url"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "access_type" && v == "offline")
    );

    let callback = format!("/oauth/google/callback?state={state}&code=auth-code");
    let request = || Request::get(&callback).body(Body::empty()).unwrap();
    let (status, body) = send(&h.app, request()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(&email.to_lowercase()), "{body}");

    let (status, _) = send(&h.app, request()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a link state must be single-use"
    );
}

#[tokio::test]
async fn rejects_requests_without_a_valid_bearer_token() {
    let h = harness().await;
    for auth in [None, Some("Bearer wrong-token")] {
        let mut request = Request::post("/mcp").header(header::CONTENT_TYPE, "application/json");
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let request = request
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        let (status, _) = send(&h.app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn links_an_account_and_searches_it_through_mcp() {
    let h = harness().await;
    link(&h, "Me@Gmail.com").await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=rt-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-2", "expires_in": 3600,
        })))
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads"))
        .and(query_param("q", "fedramp newer_than:1d"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "threads": [{ "id": "t1" }, { "id": "gone" }],
        })))
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads/t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "t1",
            "messages": [
                { "id": "m1", "snippet": "first", "labelIds": ["INBOX"],
                  "payload": { "headers": [{"name": "Subject", "value": "FedRAMP High kickoff"}] } },
                { "id": "m2", "snippet": "latest", "labelIds": ["INBOX", "UNREAD"],
                  "payload": { "headers": [{"name": "From", "value": "3pao@example.com"}] } },
            ],
        })))
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/threads/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": 404, "message": "Requested entity was not found." },
        })))
        .mount(&h.google)
        .await;

    let result = call_tool(
        &h.app,
        "gmail_search_threads",
        json!({ "query": "fedramp newer_than:1d" }),
    )
    .await;
    assert_eq!(result["isError"], false, "{result}");
    let content = &result["structuredContent"];
    assert_eq!(content["account"], "me@gmail.com");
    let threads = content["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1, "a thread deleted mid-search is skipped");
    assert_eq!(threads[0]["subject"], "FedRAMP High kickoff");
    assert_eq!(threads[0]["from"], "3pao@example.com");
    assert_eq!(threads[0]["unread"], true);
}

#[tokio::test]
async fn reports_a_revoked_account_as_needing_relink() {
    let h = harness().await;
    link(&h, "me@gmail.com").await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant", "error_description": "Token has been expired or revoked.",
        })))
        .mount(&h.google)
        .await;

    let result = call_tool(&h.app, "gmail_list_labels", json!({})).await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("must be linked again"),
        "{result}"
    );
}

#[tokio::test]
async fn refuses_ids_that_would_change_the_gmail_request_path() {
    let h = harness().await;
    link(&h, "me@gmail.com").await;
    let result = call_tool(
        &h.app,
        "gmail_get_message",
        json!({ "message_id": "../labels" }),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a Gmail ID")
    );
}

#[tokio::test]
async fn an_account_endpoint_only_reaches_its_own_account() {
    let h = harness().await;
    link(&h, "work@example.com").await;
    link(&h, "me@gmail.com").await;

    let tools = rpc(&h.app, "/mcp/work@example.com", "tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().all(|t| t["name"] != "link_account"));
    assert!(
        tools
            .iter()
            .all(|t| t["inputSchema"]["properties"].get("account").is_none())
    );

    let result = rpc(
        &h.app,
        "/mcp/work@example.com",
        "tools/call",
        json!({ "name": "gmail_list_labels", "arguments": { "account": "me@gmail.com" } }),
    )
    .await;
    assert_eq!(result["result"]["isError"], true, "{result}");

    let (status, _) = send(
        &h.app,
        authed(
            "POST",
            "/mcp/stranger@example.com",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_service_the_account_did_not_grant_asks_for_a_relink() {
    let h = harness().await;
    link_granting(&h, "me@gmail.com", "https://mail.google.com/").await;

    let result = call_tool(&h.app, "calendar_list_events", json!({})).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("has not granted calendar"), "{text}");
}

#[tokio::test]
async fn contact_search_merges_sources_keeping_the_saved_contact() {
    let h = harness().await;
    link(&h, "me@work.example").await;
    refresh_succeeds().mount(&h.google).await;

    let person = |name: &str, email: &str, resource: &str| {
        json!({ "resourceName": resource, "names": [{"displayName": name}],
                "emailAddresses": [{"value": email}] })
    };
    Mock::given(method("GET"))
        .and(path("/people/v1/people:searchContacts"))
        .and(query_param("query", "ana"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "person": person("Ana (saved)", "Ana@work.example", "people/c1") }],
        })))
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/people/v1/otherContacts:search"))
        .and(query_param("query", "ana"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "person": person("Ana B", "ana.b@vendor.example", "otherContacts/c2") }],
        })))
        .mount(&h.google)
        .await;
    Mock::given(method("GET"))
        .and(path("/people/v1/people:searchDirectoryPeople"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "people": [person("Ana Directory", "ana@work.example", "people/123")],
        })))
        .mount(&h.google)
        .await;
    // The cache warm-up sends empty queries first.
    Mock::given(method("GET"))
        .and(query_param("query", ""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&h.google)
        .await;

    let result = call_tool(&h.app, "contacts_search", json!({ "query": "ana" })).await;
    assert_eq!(result["isError"], false, "{result}");
    let people = result["structuredContent"]["people"].as_array().unwrap();
    let names: Vec<&str> = people.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["Ana (saved)", "Ana B"],
        "the directory duplicate is dropped"
    );
    assert_eq!(people[0]["source"], "contact");
}

async fn mount_labels(h: &Harness) {
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "labels": [
            {"id": "INBOX", "name": "INBOX", "type": "system"},
            {"id": "Label_7", "name": "Receipts/2026", "type": "user"},
        ]})))
        .mount(&h.google)
        .await;
}

#[tokio::test]
async fn modify_resolves_label_names_and_reports_each_thread() {
    let h = harness().await;
    link(&h, "me@gmail.com").await;
    refresh_succeeds().mount(&h.google).await;
    mount_labels(&h).await;
    Mock::given(method("POST"))
        .and(path("/gmail/v1/users/me/messages/batchModify"))
        .and(body_json(json!({
            "ids": ["m1", "m2"], "addLabelIds": ["Label_7"], "removeLabelIds": ["INBOX"],
        })))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.google)
        .await;
    Mock::given(method("POST"))
        .and(path("/gmail/v1/users/me/threads/t1/modify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "t1" })))
        .mount(&h.google)
        .await;
    Mock::given(method("POST"))
        .and(path("/gmail/v1/users/me/threads/t2/modify"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": 404, "message": "Requested entity was not found." },
        })))
        .mount(&h.google)
        .await;

    let result = call_tool(
        &h.app,
        "gmail_modify",
        json!({
            "message_ids": ["m1", "m2"],
            "thread_ids": ["t1", "t2"],
            "add_labels": ["receipts/2026"],
            "remove_labels": ["INBOX"],
        }),
    )
    .await;
    assert_eq!(result["isError"], false, "{result}");
    let content = &result["structuredContent"];
    assert_eq!(content["messages"]["succeeded"], 2, "{content}");
    assert_eq!(content["threads"]["succeeded"], 1);
    assert_eq!(content["threads"]["failed"][0]["id"], "t2");
}

#[tokio::test]
async fn system_labels_cannot_be_deleted() {
    let h = harness().await;
    link(&h, "me@gmail.com").await;
    refresh_succeeds().mount(&h.google).await;
    mount_labels(&h).await;

    let result = call_tool(&h.app, "gmail_delete_label", json!({ "label": "inbox" })).await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("system label")
    );
}
