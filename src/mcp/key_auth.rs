//! API-key authentication for the MCP endpoint, as a tower-http
//! [`AsyncRequireAuthorizationLayer`]. A request needs
//! `Authorization: Bearer <key>` (or `X-API-Key: <key>`) with a key made on
//! the dashboard's MCP page or by `localleads mcp install --write`.

use crate::app::SharedState;
use crate::auth;
use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use futures::future::BoxFuture;
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};

#[derive(Clone)]
pub struct McpKeyAuth {
    state: SharedState,
}

pub fn layer(state: SharedState) -> AsyncRequireAuthorizationLayer<McpKeyAuth> {
    AsyncRequireAuthorizationLayer::new(McpKeyAuth { state })
}

fn refuse(message: &str) -> Response<Body> {
    let body = serde_json::json!({ "error": message }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(r#"Bearer realm="LocalLeads MCP""#),
    );
    response
}

impl AsyncAuthorizeRequest<Body> for McpKeyAuth {
    type RequestBody = Body;
    type ResponseBody = Body;
    type Future = BoxFuture<'static, Result<Request<Body>, Response<Body>>>;

    fn authorize(&mut self, request: Request<Body>) -> Self::Future {
        let db = self.state.db.clone();
        Box::pin(async move {
            let Some(key) = crate::web::api_key(request.headers()) else {
                return Err(refuse(
                    "LocalLeads needs an API key. Create one on the dashboard's MCP page and send it as \"Authorization: Bearer <key>\".",
                ));
            };
            let hash = auth::digest(&key);
            match tokio::task::spawn_blocking(move || db.use_api_key(&hash)).await {
                Ok(Ok(true)) => Ok(request),
                Ok(Ok(false)) => Err(refuse("Invalid or revoked API key.")),
                _ => {
                    let mut response = Response::new(Body::from("could not check the API key"));
                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    Err(response)
                }
            }
        })
    }
}
