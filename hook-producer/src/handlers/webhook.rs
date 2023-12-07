use axum::{extract::State, http::StatusCode, Json};
use hook_common::webhook::WebhookJobParameters;
use serde_derive::Deserialize;
use url::Url;

use hook_common::pgqueue::{NewJob, PgQueue};
use serde::Serialize;
use tracing::{debug, error};

const MAX_BODY_SIZE: usize = 1_000_000;

#[derive(Serialize, Deserialize)]
pub struct WebhookPostResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn post(
    State(pg_queue): State<PgQueue>,
    Json(payload): Json<WebhookJobParameters>,
) -> Result<Json<WebhookPostResponse>, (StatusCode, Json<WebhookPostResponse>)> {
    debug!("received payload: {:?}", payload);

    if payload.body.len() > MAX_BODY_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(WebhookPostResponse {
                error: Some("body too large".to_owned()),
            }),
        ));
    }

    let url_hostname = get_hostname(&payload.url)?;
    let job = NewJob::new(payload.max_attempts, payload, url_hostname.as_str());

    pg_queue.enqueue(job).await.map_err(internal_error)?;

    Ok(Json(WebhookPostResponse { error: None }))
}

fn internal_error<E>(err: E) -> (StatusCode, Json<WebhookPostResponse>)
where
    E: std::error::Error,
{
    error!("internal error: {}", err);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(WebhookPostResponse {
            error: Some(err.to_string()),
        }),
    )
}

fn get_hostname(url_str: &str) -> Result<String, (StatusCode, Json<WebhookPostResponse>)> {
    let url = Url::parse(url_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(WebhookPostResponse {
                error: Some("could not parse url".to_owned()),
            }),
        )
    })?;

    match url.host_str() {
        Some(hostname) => Ok(hostname.to_owned()),
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(WebhookPostResponse {
                error: Some("couldn't extract hostname from url".to_owned()),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use hook_common::pgqueue::{PgQueue, RetryPolicy};
    use hook_common::webhook::{HttpMethod, WebhookJobParameters};
    use http_body_util::BodyExt; // for `collect`
    use std::collections;
    use tower::ServiceExt; // for `call`, `oneshot`, and `ready`

    use crate::handlers::app;

    #[tokio::test]
    async fn webhook_success() {
        let pg_queue = PgQueue::new(
            "test_index",
            "job_queue",
            "postgres://posthog:posthog@localhost:15432/test_database",
            RetryPolicy::default(),
        )
        .await
        .expect("failed to construct pg_queue");

        let app = app(pg_queue, None);

        let mut headers = collections::HashMap::new();
        headers.insert("Content-Type".to_owned(), "application/json".to_owned());
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_string(&WebhookJobParameters {
                            headers,
                            method: HttpMethod::POST,
                            url: "http://example.com/".to_owned(),
                            body: r#"{"a": "b"}"#.to_owned(),

                            team_id: Some(1),
                            plugin_id: Some(2),
                            plugin_config_id: Some(3),

                            max_attempts: 1,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"{}");
    }

    #[tokio::test]
    async fn webhook_bad_url() {
        let pg_queue = PgQueue::new(
            "test_index",
            "job_queue",
            "postgres://posthog:posthog@localhost:15432/test_database",
            RetryPolicy::default(),
        )
        .await
        .expect("failed to construct pg_queue");

        let app = app(pg_queue, None);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_string(&WebhookJobParameters {
                            headers: collections::HashMap::new(),
                            method: HttpMethod::POST,
                            url: "invalid".to_owned(),
                            body: r#"{"a": "b"}"#.to_owned(),

                            team_id: Some(1),
                            plugin_id: Some(2),
                            plugin_config_id: Some(3),

                            max_attempts: 1,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_payload_missing_fields() {
        let pg_queue = PgQueue::new(
            "test_index",
            "job_queue",
            "postgres://posthog:posthog@localhost:15432/test_database",
            RetryPolicy::default(),
        )
        .await
        .expect("failed to construct pg_queue");

        let app = app(pg_queue, None);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body("{}".to_owned())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn webhook_payload_not_json() {
        let pg_queue = PgQueue::new(
            "test_index",
            "job_queue",
            "postgres://posthog:posthog@localhost:15432/test_database",
            RetryPolicy::default(),
        )
        .await
        .expect("failed to construct pg_queue");

        let app = app(pg_queue, None);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body("x".to_owned())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_payload_body_too_large() {
        let pg_queue = PgQueue::new(
            "test_index",
            "job_queue",
            "postgres://posthog:posthog@localhost:15432/test_database",
            RetryPolicy::default(),
        )
        .await
        .expect("failed to construct pg_queue");

        let app = app(pg_queue, None);

        let bytes: Vec<u8> = vec![b'a'; 1_000_000 * 2];
        let long_string = String::from_utf8_lossy(&bytes);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/webhook")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_string(&WebhookJobParameters {
                            headers: collections::HashMap::new(),
                            method: HttpMethod::POST,
                            url: "http://example.com".to_owned(),
                            body: long_string.to_string(),

                            team_id: Some(1),
                            plugin_id: Some(2),
                            plugin_config_id: Some(3),

                            max_attempts: 1,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
