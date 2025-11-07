use std::time::{Duration, SystemTime};

use mm_auth::testing::{Issuer, fixed_now, tampered_signature, unix};
use mm_auth::{AuthError, Settings, Verifier};

fn now() -> SystemTime {
    fixed_now()
}

#[test]
fn a_token_signed_by_a_published_key_carries_its_subject() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = issuer.token("owner-1", now(), Duration::from_secs(300));
    let identity = verifier.verify(&token, now()).expect("the token verifies");
    assert_eq!(identity.subject(), "owner-1");
}

#[test]
fn a_signature_that_does_not_match_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = tampered_signature(&issuer, "owner-1");
    let error = verifier
        .verify(&token, now())
        .expect_err("a forged token verifies");
    assert_eq!(error, AuthError::BadSignature);
}

#[test]
fn a_signature_from_another_key_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let other = Issuer::seeded("key-1", 9);
    let verifier =
        Verifier::from_jwks(&other.jwks(), Settings::none()).expect("the published key set loads");
    let token = issuer.token("owner-1", now(), Duration::from_secs(300));
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("a foreign token verifies"),
        AuthError::BadSignature
    );
}

#[test]
fn a_symmetric_algorithm_is_refused_even_when_the_issuer_offers_it() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = issuer.symmetric_token("owner-1", now(), Duration::from_secs(300));
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("a shared-secret token verifies"),
        AuthError::UnsupportedAlgorithm
    );
}

#[test]
fn a_published_shared_secret_is_refused_at_load_time() {
    let document =
        r#"{"keys":[{"kty":"oct","kid":"key-1","alg":"HS256","k":"c2hhcmVkLXNlY3JldA"}]}"#;
    let error = Verifier::from_jwks(document, Settings::none())
        .expect_err("a shared key loads as a public one");
    assert!(
        matches!(error, mm_auth::KeySetError::UnsupportedKey { .. }),
        "{error}"
    );
}

#[test]
fn an_expired_token_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = issuer.token("owner-1", now(), Duration::from_secs(60));
    assert!(verifier.verify(&token, now()).is_ok());
    let later = now() + Duration::from_secs(61);
    assert_eq!(
        verifier
            .verify(&token, later)
            .expect_err("an expired token verifies"),
        AuthError::Expired
    );
}

#[test]
fn a_token_that_is_not_yet_valid_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = issuer.token(
        "owner-1",
        now() + Duration::from_secs(600),
        Duration::from_secs(300),
    );
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("a future token verifies"),
        AuthError::NotYetValid
    );
}

#[test]
fn the_clock_is_granted_a_bounded_leeway() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier = Verifier::from_jwks(
        &issuer.jwks(),
        Settings::none().with_leeway(Duration::from_secs(30)),
    )
    .expect("the published key set loads");
    let token = issuer.token("owner-1", now(), Duration::from_secs(60));
    assert!(
        verifier
            .verify(&token, now() + Duration::from_secs(75))
            .is_ok(),
        "a token inside the leeway must still verify"
    );
    assert_eq!(
        verifier
            .verify(&token, now() + Duration::from_secs(95))
            .expect_err("a token past the leeway verifies"),
        AuthError::Expired
    );
}

#[test]
fn an_unknown_key_id_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let other = Issuer::seeded("key-2", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = other.token("owner-1", now(), Duration::from_secs(300));
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("an unknown key verifies"),
        AuthError::UnknownKey
    );
}

#[test]
fn an_issuer_that_is_not_the_expected_one_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier = Verifier::from_jwks(&issuer.jwks(), Settings::none().issued_by("other-issuer"))
        .expect("the key set loads");
    let token = issuer.token("owner-1", now(), Duration::from_secs(300));
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("a foreign issuer verifies"),
        AuthError::IssuerMismatch
    );
}

#[test]
fn a_token_without_a_subject_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let issued = unix(now());
    let token = issuer.token_with_claims(&format!(
        "{{\"iss\":\"test-issuer\",\"exp\":{}}}",
        issued + 300
    ));
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("a subjectless token verifies"),
        AuthError::SubjectMissing
    );
}

#[test]
fn a_token_that_is_not_three_parts_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    assert_eq!(
        verifier
            .verify("not-a-token", now())
            .expect_err("a malformed token verifies"),
        AuthError::Malformed
    );
}

#[test]
fn an_unreadable_part_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let token = format!("{}.{}.{}", "!!!", "!!!", "!!!");
    assert_eq!(
        verifier
            .verify(&token, now())
            .expect_err("an undecodable token verifies"),
        AuthError::Malformed
    );
}

#[test]
fn a_key_set_without_usable_keys_is_refused() {
    assert!(Verifier::from_jwks(r#"{"keys":[]}"#, Settings::none()).is_err());
    assert!(Verifier::from_jwks("not json", Settings::none()).is_err());
}

#[test]
fn two_published_keys_are_both_accepted_and_an_unknown_one_is_not() {
    let first = Issuer::seeded("key-1", 7);
    let second = Issuer::seeded("key-2", 11);
    let document = format!(
        r#"{{"keys":[{},{}]}}"#,
        trim_document(&first.jwks()),
        trim_document(&second.jwks())
    );
    let verifier =
        Verifier::from_jwks(&document, Settings::none()).expect("the published key set loads");
    assert_eq!(
        verifier
            .verify(
                &first.token("owner-1", now(), Duration::from_secs(60)),
                now()
            )
            .expect("the first key signs")
            .subject(),
        "owner-1"
    );
    assert_eq!(
        verifier
            .verify(
                &second.token("owner-2", now(), Duration::from_secs(60)),
                now()
            )
            .expect("the second key signs")
            .subject(),
        "owner-2"
    );
}

fn trim_document(document: &str) -> String {
    document
        .trim_start_matches(r#"{"keys":["#)
        .trim_end_matches(r#"]}"#)
        .to_string()
}

#[test]
fn the_bearer_scheme_is_required_and_the_scheme_is_read_case_insensitively() {
    let issuer = Issuer::seeded("key-1", 7);
    let token = issuer.token("owner-1", now(), Duration::from_secs(60));
    assert_eq!(
        mm_auth::bearer_token(format!("Basic {token}").as_str())
            .expect_err("a non-bearer scheme is accepted"),
        AuthError::UnsupportedScheme
    );
    assert_eq!(
        mm_auth::bearer_token(&token).expect_err("a bare token is accepted"),
        AuthError::UnsupportedScheme
    );
    assert_eq!(
        mm_auth::bearer_token(&format!("bearer {token}"))
            .expect("a lower-case scheme must be read")
            .to_string(),
        token
    );
}
