use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::{config::gateway_dto::SgProtocol, plugins::context::SgRoutePluginContext};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::{client::HttpConnector, Body, Client, Error};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use tardis::{
    basic::{error::TardisError, result::TardisResult},
    log,
    tokio::time::timeout,
};

const DEFAULT_TIMEOUT_MS: u64 = 5000;

static DEFAULT_CLIENT: OnceLock<Client<HttpsConnector<HttpConnector>>> = OnceLock::new();

pub fn init() -> TardisResult<&'static Client<HttpsConnector<HttpConnector>>> {
    if DEFAULT_CLIENT.get().is_none() {
        let _ = DEFAULT_CLIENT.set(do_init(false)?);
    }
    Ok(default_client())
}

pub fn get_ignore_validation_clint() -> TardisResult<Client<HttpsConnector<HttpConnector>>> {
    do_init(true)
}

fn do_init(ignore_validation: bool) -> TardisResult<Client<HttpsConnector<HttpConnector>>> {
    fn get_tls_config(ignore: bool) -> rustls::ClientConfig {
        if ignore {
            get_rustls_config_dangerous()
        } else {
            rustls::ClientConfig::builder().with_safe_defaults().with_native_roots().with_no_client_auth()
        }
    }

    let https = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(get_tls_config(ignore_validation)).https_or_http().enable_http1().build();
    let tls_client = Client::builder().build(https);

    Ok(tls_client)
}

pub fn get_rustls_config_dangerous() -> rustls::ClientConfig {
    let store = rustls::RootCertStore::empty();
    let mut config = rustls::ClientConfig::builder().with_safe_defaults().with_root_certificates(store).with_no_client_auth();

    // completely disable cert-verification
    let mut dangerous_config = rustls::ClientConfig::dangerous(&mut config);
    dangerous_config.set_certificate_verifier(Arc::new(NoCertificateVerification {}));

    config
}

pub struct NoCertificateVerification {}
impl rustls::client::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

#[inline]
fn default_client() -> &'static Client<HttpsConnector<HttpConnector>> {
    DEFAULT_CLIENT.get().expect("DEFAULT_CLIENT not initialized")
}

pub async fn request(
    client: &Client<HttpsConnector<HttpConnector>>,
    rule_timeout_ms: Option<u64>,
    redirect: bool,
    mut ctx: SgRoutePluginContext,
) -> TardisResult<SgRoutePluginContext> {
    if redirect {
        ctx = do_request(client, &ctx.request.get_uri().to_string(), rule_timeout_ms, ctx).await?;
    }
    if let Some(backend) = ctx.get_chose_backend() {
        let scheme = backend.protocol.as_ref().unwrap_or(&SgProtocol::Http);
        let host = format!("{}{}", backend.name_or_host, backend.namespace.as_ref().map(|n| format!(".{n}")).unwrap_or("".to_string()));
        let port = if (backend.port == 0 || backend.port == 80) && scheme == &SgProtocol::Http || (backend.port == 0 || backend.port == 443) && scheme == &SgProtocol::Https {
            "".to_string()
        } else {
            format!(":{}", backend.port)
        };
        let url = format!("{}://{}{}{}", scheme, host, port, ctx.request.get_uri().path_and_query().map(|p| p.as_str()).unwrap_or(""));
        let timeout_ms = if let Some(timeout_ms) = backend.timeout_ms { Some(timeout_ms) } else { rule_timeout_ms };
        ctx = do_request(client, &url, timeout_ms, ctx).await?;
        ctx.set_chose_backend(backend);
    }
    Ok(ctx)
}

async fn do_request(client: &Client<HttpsConnector<HttpConnector>>, url: &str, timeout_ms: Option<u64>, mut ctx: SgRoutePluginContext) -> TardisResult<SgRoutePluginContext> {
    let ctx = match raw_request(
        Some(client),
        ctx.request.get_method().clone(),
        url,
        ctx.request.take_body(),
        ctx.request.get_headers(),
        timeout_ms,
    )
    .await
    {
        Ok(response) => ctx.resp(response.status(), response.headers().clone(), response.into_body()),
        Err(e) => ctx.resp_from_error(e),
    };
    Ok(ctx)
}

pub async fn raw_request(
    client: Option<&Client<HttpsConnector<HttpConnector>>>,
    method: Method,
    url: &str,
    body: Body,
    headers: &HeaderMap<HeaderValue>,
    timeout_ms: Option<u64>,
) -> TardisResult<Response<Body>> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let method_str = method.to_string();
    let url_str = url.to_string();

    if log::level_enabled!(log::Level::TRACE) {
        log::trace!("[SG.Client] Request method {method_str} url {url_str} header {headers:?} {body:?}, timeout {timeout_ms} ms",);
    } else if log::level_enabled!(log::Level::DEBUG) {
        log::debug!("[SG.Client] Request method {method_str} url {url_str} header {headers:?}, timeout {timeout_ms} ms",);
    }

    let mut req = Request::builder();
    req = req.method(method);
    for (k, v) in headers {
        req = req.header(
            k.as_str(),
            v.to_str().map_err(|_| TardisError::bad_request(&format!("Header {} value is illegal: is not ascii", k), ""))?,
        );
    }
    req = req.uri(url);
    let req = req.body(body).map_err(|error| TardisError::internal_error(&format!("[SG.Route] Build request method {method_str} url {url_str} error:{error}"), ""))?;
    let req = if let Some(client) = client { client.request(req) } else { init()?.request(req) };
    let response = match timeout(Duration::from_millis(timeout_ms), req).await {
        Ok(response) => response.map_err(|error: Error| TardisError::custom("502", &format!("[SG.Client] Request method {method_str} url {url_str} error: {error}"), "")),
        Err(_) => {
            Response::builder().status(StatusCode::GATEWAY_TIMEOUT).body(Body::empty()).map_err(|e| TardisError::internal_error(&format!("[SG.Client] timeout error: {e}"), ""))
        }
    }?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Method, Uri, Version};
    use hyper::Body;
    use tardis::{basic::result::TardisResult, tokio};

    use crate::plugins::context::AvailableBackendInst;
    use crate::{
        config::gateway_dto::SgProtocol,
        functions::http_client::{init, request},
        plugins::context::SgRoutePluginContext,
    };
    use hyper::{client::HttpConnector, Client};
    use hyper_rustls::HttpsConnector;

    #[tokio::test]
    async fn test_request() -> TardisResult<()> {
        let client = init().unwrap();

        // test simple
        let mut resp = retry_test_request(
            client,
            None,
            false,
            SgRoutePluginContext::new_http(
                Method::GET,
                Uri::from_static("http://sg.idealworld.group"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::empty(),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                Some(AvailableBackendInst {
                    name_or_host: "www.baidu.com".to_string(),
                    port: 80,
                    ..Default::default()
                }),
            ),
        )
        .await?;
        assert_eq!(resp.response.get_status_code().as_u16(), 200);
        let body = String::from_utf8(resp.response.dump_body().await?.to_vec()).unwrap();
        assert!(body.contains("百度一下"));

        // test get
        let mut resp = retry_test_request(
            client,
            Some(20000),
            false,
            SgRoutePluginContext::new_http(
                Method::GET,
                Uri::from_static("http://sg.idealworld.group/get?foo1=bar1&foo2=bar2"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::empty(),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                Some(AvailableBackendInst {
                    name_or_host: "httpbin.org".to_string(),
                    port: 80,
                    ..Default::default()
                }),
            ),
        )
        .await?;
        assert_eq!(resp.response.get_status_code().as_u16(), 200);
        let body = String::from_utf8(resp.response.dump_body().await?.to_vec()).unwrap();
        assert!(body.contains(r#""url": "http://httpbin.org/get?foo1=bar1&foo2=bar2""#));

        // test post with tls
        let mut resp = retry_test_request(
            client,
            Some(20000),
            false,
            SgRoutePluginContext::new_http(
                Method::POST,
                Uri::from_static("http://sg.idealworld.group/post?foo1=bar1&foo2=bar2"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::from("星航".as_bytes()),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                Some(AvailableBackendInst {
                    name_or_host: "postman-echo.com".to_string(),
                    protocol: Some(SgProtocol::Https),
                    port: 443,
                    ..Default::default()
                }),
            ),
        )
        .await?;
        assert_eq!(resp.response.get_status_code().as_u16(), 200);
        let body = String::from_utf8(resp.response.dump_body().await?.to_vec()).unwrap();
        assert!(body.contains(r#""url": "https://postman-echo.com/post?foo1=bar1&foo2=bar2""#));
        assert!(body.contains(r#""data": "星航""#));

        // test timeout
        let resp = retry_test_request(
            client,
            Some(5),
            false,
            SgRoutePluginContext::new_http(
                Method::GET,
                Uri::from_static("http://sg.idealworld.group/get?foo1=bar1&foo2=bar2"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::empty(),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                Some(AvailableBackendInst {
                    name_or_host: "postman-echo.com".to_string(),
                    port: 80,
                    ..Default::default()
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.response.get_status_code().as_u16(), 504);

        let mut resp = retry_test_request(
            client,
            Some(20000),
            false,
            SgRoutePluginContext::new_http(
                Method::GET,
                Uri::from_static("http://sg.idealworld.group/get?foo1=bar1&foo2=bar2"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::empty(),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                Some(AvailableBackendInst {
                    name_or_host: "postman-echo.com".to_string(),
                    port: 443,
                    protocol: Some(SgProtocol::Https),
                    timeout_ms: Some(20000),
                    ..Default::default()
                }),
            ),
        )
        .await?;
        assert_eq!(resp.response.get_status_code().as_u16(), 200);
        let body = String::from_utf8(resp.response.dump_body().await?.to_vec()).unwrap();
        assert!(body.contains(r#""url": "https://postman-echo.com/get?foo1=bar1&foo2=bar2""#));

        // test redirect
        let mut resp = retry_test_request(
            client,
            Some(20000),
            true,
            SgRoutePluginContext::new_http(
                Method::GET,
                Uri::from_static("https://postman-echo.com/get?foo1=bar1&foo2=bar2"),
                Version::HTTP_11,
                HeaderMap::new(),
                Body::empty(),
                "127.0.0.1:8080".parse().unwrap(),
                "".to_string(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.response.get_status_code().as_u16(), 200);
        let body = String::from_utf8(resp.response.dump_body().await?.to_vec()).unwrap();
        assert!(body.contains(r#""url": "https://postman-echo.com/get?foo1=bar1&foo2=bar2""#));

        Ok(())
    }

    // Because this unit test depends on the external url,
    // it may be due to the failure of the external url, so add retry
    async fn retry_test_request(
        client: &Client<HttpsConnector<HttpConnector>>,
        rule_timeout_ms: Option<u64>,
        redirect: bool,
        mut ctx: SgRoutePluginContext,
    ) -> TardisResult<SgRoutePluginContext> {
        let clone_body = ctx.request.dump_body().await?;
        let mut clone_ctx = ctx.clone();
        clone_ctx.request.set_body(clone_body);
        let mut result = request(client, rule_timeout_ms, redirect, ctx).await?;
        if !result.response.get_status_code().is_success() {
            result = request(client, rule_timeout_ms, redirect, clone_ctx).await?;
        }
        Ok(result)
    }
}
