//! The two halves agree: a token issued by auth-service verifies with `common::TokenVerifier`.
mod support;
use support::TestApp;

#[tokio::test]
async fn common_verifier_accepts_issued_access_token() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let tokens = app.oauth_tokens(&session, "openid profile").await;
    let cfg = &app.state.cfg;
    let verifier = common::TokenVerifier::new(cfg.issuer.clone(), cfg.audience.clone());

    let claims = verifier
        .verify(tokens["access_token"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(claims.preferred_username, "wiertmir");
    assert!(claims.has_scope("openid") && claims.has_scope("profile"));

    // an ID token (aud = client id) must not pass as an access token
    let id = tokens["id_token"].as_str().unwrap();
    assert_eq!(verifier.verify(id).await.unwrap_err().code, "unauthorized");
}
