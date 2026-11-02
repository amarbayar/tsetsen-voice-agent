//! The Google adapters against a local fake of Google's endpoints.

use std::sync::Arc;
use std::time::Duration;

use tsetsen_core::ports::{Contacts, Mailbox, Mailer, OutgoingMail};
use tsetsen_core::{Ctx, RequestId, ServiceError};
use tsetsen_google::{
    Auth, CredentialStore, Credentials, Endpoints, GmailMailer, GoogleContacts, InMemoryStore,
    OAuthClient, Secret, sign_in,
};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(10))
}

fn client() -> OAuthClient {
    OAuthClient {
        client_id: "id.apps.googleusercontent.com".into(),
        client_secret: Secret::new("shh"),
    }
}

fn signed_in() -> Arc<InMemoryStore> {
    let store = Arc::new(InMemoryStore::default());
    store
        .save(&Credentials {
            client: client(),
            refresh_token: Secret::new("refresh-1"),
        })
        .unwrap();
    store
}

async fn token_endpoint(server: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"access_token": token, "expires_in": 3599})),
        )
        .mount(server)
        .await;
}

fn auth(server: &MockServer, store: Arc<InMemoryStore>) -> Arc<Auth> {
    Arc::new(Auth::new(
        reqwest::Client::new(),
        Endpoints::at(&server.uri()),
        store,
    ))
}

fn mail(attachments: Vec<std::path::PathBuf>) -> OutgoingMail {
    OutgoingMail {
        to: vec![Mailbox {
            name: Some("David Smith".into()),
            address: "david@example.com".into(),
        }],
        subject: "Our trip".into(),
        body: "Here they are. Love, Mom".into(),
        attachments,
    }
}

#[tokio::test]
async fn sends_the_message_with_its_pictures() {
    let server = MockServer::start().await;
    token_endpoint(&server, "access-1").await;
    Mock::given(method("POST"))
        .and(path("/upload/gmail/v1/users/me/messages/send"))
        .and(query_param("uploadType", "media"))
        .and(header("authorization", "Bearer access-1"))
        .and(header("content-type", "message/rfc822"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m-1"})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = std::env::temp_dir().join(format!("tsetsen-google-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let picture = dir.join("picture-01.jpg");
    std::fs::write(&picture, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();

    let mailer = GmailMailer::new(auth(&server, signed_in()));
    let sent = mailer.send(&mail(vec![picture]), &cx()).await.unwrap();
    assert_eq!(sent.id, "m-1");

    let requests = server.received_requests().await.unwrap();
    let raw = String::from_utf8(requests.last().unwrap().body.clone()).unwrap();
    assert!(raw.starts_with("To: =?UTF-8?B?"), "{raw}");
    assert!(raw.contains("<david@example.com>"));
    assert!(raw.contains("filename=\"picture-01.jpg\""));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_rejected_token_is_refreshed_once_and_retried() {
    let server = MockServer::start().await;
    token_endpoint(&server, "access-1").await;
    Mock::given(method("POST"))
        .and(path("/upload/gmail/v1/users/me/messages/send"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/gmail/v1/users/me/messages/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m-2"})))
        .mount(&server)
        .await;
    let mailer = GmailMailer::new(auth(&server, signed_in()));
    assert_eq!(mailer.send(&mail(vec![]), &cx()).await.unwrap().id, "m-2");
    let refreshes = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r: &&Request| r.url.path() == "/token")
        .count();
    assert_eq!(refreshes, 2, "the cached token was dropped and refreshed");
}

#[tokio::test]
async fn failures_say_whether_the_message_may_have_gone() {
    for (status, expected) in [
        (500, "uncertain"),
        (503, "unavailable"),
        (400, "rejected"),
        (403, "denied"),
    ] {
        let server = MockServer::start().await;
        token_endpoint(&server, "access-1").await;
        Mock::given(method("POST"))
            .and(path("/upload/gmail/v1/users/me/messages/send"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let error = GmailMailer::new(auth(&server, signed_in()))
            .send(&mail(vec![]), &cx())
            .await
            .unwrap_err();
        let kind = match error {
            ServiceError::Uncertain(_) => "uncertain",
            ServiceError::Unavailable(_) => "unavailable",
            ServiceError::Rejected(_) => "rejected",
            ServiceError::PermissionDenied(_) => "denied",
            _ => "other",
        };
        assert_eq!(kind, expected, "{status}");
    }
}

#[tokio::test]
async fn nothing_is_sent_without_a_sign_in_or_to_a_bad_address() {
    let server = MockServer::start().await;
    let unsigned = GmailMailer::new(auth(&server, Arc::new(InMemoryStore::default())));
    assert!(matches!(
        unsigned.send(&mail(vec![]), &cx()).await,
        Err(ServiceError::NotSetUp(_))
    ));
    let mut bad = mail(vec![]);
    bad.to[0].address = "david@example.com>, evil@example.com".into();
    assert!(matches!(
        GmailMailer::new(auth(&server, signed_in()))
            .send(&bad, &cx())
            .await,
        Err(ServiceError::Rejected(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn finds_contacts_and_people_written_to() {
    let server = MockServer::start().await;
    token_endpoint(&server, "access-1").await;
    let person = |name: &str, email: &str, id: &str| {
        serde_json::json!({"person": {"resourceName": id, "names": [{"displayName": name}],
            "emailAddresses": [{"value": email}]}})
    };
    Mock::given(method("GET"))
        .and(path("/v1/people:searchContacts"))
        .and(query_param("query", "david"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [person("David Smith", "david@example.com", "people/c1")]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/otherContacts:search"))
        .and(query_param("query", "david"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [person("David Smith", "david@example.com", "otherContacts/x"),
                        person("David Lee", "dlee@example.com", "otherContacts/y")]})))
        .mount(&server)
        .await;
    // The warm-up searches.
    Mock::given(method("GET"))
        .and(query_param("query", ""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;
    let contacts = GoogleContacts::new(auth(&server, signed_in()));
    let found = contacts.search("david", &cx()).await.unwrap();
    let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["David Smith", "David Lee"], "duplicates merged");
    contacts.search("david", &cx()).await.unwrap();
}

#[tokio::test]
async fn signs_in_through_the_browser_with_pkce() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"access_token": "a", "expires_in": 3599, "refresh_token": "r-1"}),
        ))
        .mount(&server)
        .await;
    let store = InMemoryStore::default();
    let endpoints = Endpoints::at(&server.uri());
    // The "browser": follows the redirect with the code and the state.
    let browser = |consent: &str| {
        let consent = url::Url::parse(consent).unwrap();
        let get = |k: &str| {
            consent
                .query_pairs()
                .find(|(key, _)| key == k)
                .unwrap()
                .1
                .into_owned()
        };
        assert_eq!(get("code_challenge_method"), "S256");
        assert!(get("scope").contains("gmail.send"));
        assert!(!get("scope").contains("gmail.readonly"), "never reads mail");
        let redirect = format!(
            "{}/?state={}&code=the-code",
            get("redirect_uri"),
            get("state")
        );
        tokio::spawn(async move {
            // A stray request first, as browsers make.
            let _ = reqwest::get(format!("{}/favicon.ico", get_origin(&redirect))).await;
            reqwest::get(redirect).await.unwrap();
        });
    };
    sign_in(client(), &store, &endpoints, browser)
        .await
        .unwrap();
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.refresh_token, Secret::new("r-1"));
}

fn get_origin(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();
    format!(
        "http://{}:{}",
        parsed.host_str().unwrap(),
        parsed.port().unwrap()
    )
}
