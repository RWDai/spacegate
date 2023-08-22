use std::net::IpAddr;
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use http::Request;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};

use serde::{Deserialize, Serialize};
use tardis::chrono::Utc;
use tardis::tokio::task::JoinHandle;
use tardis::{
    basic::result::TardisResult,
    log,
    tokio::{
        self,
        sync::{watch::Sender, Mutex},
    },
    TardisFuns,
};

use crate::functions::cache_client;

use self::status_plugin::{clean_status, get_status, update_status};

use super::{BoxSgPluginFilter, SgAttachedLevel, SgPluginFilter, SgPluginFilterDef, SgPluginFilterInitDto, SgRoutePluginContext};
use lazy_static::lazy_static;
use tardis::basic::error::TardisError;

lazy_static! {
    static ref SHUTDOWN_TX: Arc<Mutex<HashMap<u16, (Sender<()>, JoinHandle<Result<(), hyper::Error>>)>>> = <_>::default();
    static ref SERVER_ERR: Arc<Mutex<HashMap<String, (u16, i64)>>> = <_>::default();
}

pub mod status_plugin;

pub const CODE: &str = "status";
pub struct SgFilterStatusDef;

impl SgPluginFilterDef for SgFilterStatusDef {
    fn inst(&self, spec: serde_json::Value) -> TardisResult<BoxSgPluginFilter> {
        let filter = TardisFuns::json.json_to_obj::<SgFilterStatus>(spec)?;
        Ok(filter.boxed())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct SgFilterStatus {
    pub serv_addr: String,
    pub port: u16,
    pub title: String,
    /// Unhealthy threshold , if server error more than this, server will be tag as unhealthy
    pub unhealthy_threshold: u16,
    pub interval: u64,
    pub cache_key: String,
}

impl Default for SgFilterStatus {
    fn default() -> Self {
        Self {
            serv_addr: "0.0.0.0".to_string(),
            port: 8110,
            title: "System Status".to_string(),
            unhealthy_threshold: 3,
            interval: 5,
            cache_key: "spacegate:cache:plugin:status".to_string(),
        }
    }
}

#[async_trait]
impl SgPluginFilter for SgFilterStatus {
    fn accept(&self) -> super::SgPluginFilterAccept {
        super::SgPluginFilterAccept {
            kind: vec![super::SgPluginFilterKind::Http],
            accept_error_response: true,
        }
    }

    async fn init(&mut self, init_dto: &SgPluginFilterInitDto) -> TardisResult<()> {
        if !init_dto.attached_level.eq(&SgAttachedLevel::Gateway) {
            log::error!("[SG.Filter.Status] init filter is only can attached to gateway");
            return Ok(());
        }
        let (shutdown_tx, _) = tokio::sync::watch::channel(());
        let mut shutdown_rx = shutdown_tx.subscribe();

        let mut shutdown = SHUTDOWN_TX.lock().await;
        if let Some(old_shutdown) = shutdown.remove(&self.port) {
            old_shutdown.0.send(()).ok();
            let _ = old_shutdown.1.await;
            log::trace!("[SG.Filter.Status] init stop old service.");
        }

        let addr_ip: IpAddr = self.serv_addr.parse().map_err(|e| TardisError::conflict(&format!("[SG.Filter.Status] serv_addr parse error: {e}"), ""))?;
        let addr = (addr_ip, self.port).into();
        let title = Arc::new(Mutex::new(self.title.clone()));
        let gateway_name = Arc::new(Mutex::new(init_dto.gateway_name.clone()));
        let cache_key = Arc::new(Mutex::new(get_cache_key(&self.cache_key, &init_dto.gateway_name)));
        let make_svc = make_service_fn(move |_conn| {
            let title = title.clone();
            let gateway_name = gateway_name.clone();
            let cache_key = cache_key.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |request: Request<Body>| {
                    status_plugin::create_status_html(request, gateway_name.clone(), cache_key.clone(), title.clone())
                }))
            }
        });

        let server = match Server::try_bind(&addr) {
            Ok(server) => server.serve(make_svc),
            Err(e) => return Err(TardisError::conflict(&format!("[SG.Filter.Status] bind error: {e}"), "")),
        };

        let join = tokio::spawn(async move {
            log::info!("[SG.Filter.Status] Server started: {addr}");
            let server = server.with_graceful_shutdown(async move {
                shutdown_rx.changed().await.ok();
            });
            server.await
        });
        (*shutdown).insert(self.port, (shutdown_tx, join));

        let cache_client = cache_client::get(&init_dto.gateway_name)?;

        clean_status(&get_cache_key(&self.cache_key, &init_dto.gateway_name), cache_client).await?;
        for http_route_rule in init_dto.http_route_rules.clone() {
            if let Some(backends) = &http_route_rule.backends {
                for backend in backends {
                    update_status(
                        &backend.name_or_host,
                        &get_cache_key(&self.cache_key, &init_dto.gateway_name),
                        cache_client,
                        status_plugin::Status::default(),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn destroy(&self) -> TardisResult<()> {
        let mut shutdown = SHUTDOWN_TX.lock().await;

        if let Some(shutdown) = shutdown.remove(&self.port) {
            shutdown.0.send(()).ok();
            let _ = shutdown.1.await;
            log::info!("[SG.Filter.Status] Server stopped");
        };
        Ok(())
    }

    async fn req_filter(&self, _: &str, ctx: SgRoutePluginContext) -> TardisResult<(bool, SgRoutePluginContext)> {
        Ok((true, ctx))
    }

    async fn resp_filter(&self, _: &str, ctx: SgRoutePluginContext) -> TardisResult<(bool, SgRoutePluginContext)> {
        if let Some(backend_name) = ctx.get_chose_backend_name() {
            if ctx.is_resp_error() {
                let mut server_err = SERVER_ERR.lock().await;
                let now = Utc::now().timestamp();
                if let Some((times, expire)) = server_err.get_mut(&backend_name) {
                    println!("[SG.Filter.Status] times:{times} expire:{expire} now:{now} unhealthy");
                    if *expire > now {
                        if *times >= self.unhealthy_threshold {
                            update_status(
                                &backend_name,
                                &get_cache_key(&self.cache_key, &ctx.get_gateway_name()),
                                ctx.cache()?,
                                status_plugin::Status::Major,
                            )
                            .await?;
                        } else {
                            update_status(
                                &backend_name,
                                &get_cache_key(&self.cache_key, &ctx.get_gateway_name()),
                                ctx.cache()?,
                                status_plugin::Status::Minor,
                            )
                            .await?;
                        }
                        let new_times = *times + 1;
                        server_err.insert(backend_name.clone(), (new_times, now + self.interval as i64));
                    } else {
                        server_err.insert(backend_name.clone(), (1, now + self.interval as i64));
                    }
                } else {
                    update_status(
                        &backend_name,
                        &get_cache_key(&self.cache_key, &ctx.get_gateway_name()),
                        ctx.cache()?,
                        status_plugin::Status::Minor,
                    )
                    .await?;
                    server_err.insert(backend_name.clone(), (1, now + self.interval as i64));
                }
            } else if let Some(status) = get_status(&backend_name, &get_cache_key(&self.cache_key, &ctx.get_gateway_name()), ctx.cache()?).await? {
                if status != status_plugin::Status::Good {
                    update_status(
                        &backend_name,
                        &get_cache_key(&self.cache_key, &ctx.get_gateway_name()),
                        ctx.cache()?,
                        status_plugin::Status::Good,
                    )
                    .await?;
                }
            }
        }
        Ok((true, ctx))
    }
}
fn get_cache_key(cache_key: &str, gateway_name: &str) -> String {
    format!("{}:{}", cache_key, gateway_name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::env;

    use http::{HeaderMap, Method, StatusCode, Uri, Version};
    use hyper::Body;

    use tardis::{
        basic::{error::TardisError, result::TardisResult},
        test::test_container::TardisTestContainer,
        testcontainers::{self, clients::Cli, images::redis::Redis, Container},
        tokio,
    };

    use crate::{
        config::{
            gateway_dto::SgParameters,
            http_route_dto::{SgBackendRef, SgHttpRouteRule},
        },
        functions,
        instance::{SgBackendInst, SgHttpRouteRuleInst},
        plugins::{
            context::ChosenHttpRouteRuleInst,
            filters::{
                status::{
                    get_cache_key,
                    status_plugin::{get_status, Status},
                    SgFilterStatus,
                },
                SgPluginFilter, SgPluginFilterInitDto, SgRoutePluginContext,
            },
        },
    };

    #[tokio::test]
    async fn test_status() {
        tracing_subscriber::fmt::init();
        let mut stats = SgFilterStatus::default();
        let mock_backend_ref = SgBackendRef {
            name_or_host: "test1".to_string(),
            namespace: None,
            port: 80,
            timeout_ms: None,
            protocol: Some(crate::config::gateway_dto::SgProtocol::Http),
            weight: None,
            filters: None,
        };
        let docker = testcontainers::clients::Cli::default();
        let _x = docker_init(&docker).await.unwrap();
        let gateway_name = "gateway_name1".to_string();
        functions::cache_client::init(&gateway_name, &env::var("TARDIS_FW.CACHE.URL").unwrap()).await.unwrap();

        stats
            .init(&SgPluginFilterInitDto {
                gateway_name: gateway_name.clone(),
                gateway_parameters: SgParameters::default(),
                http_route_rules: vec![SgHttpRouteRule {
                    matches: None,
                    filters: None,
                    backends: Some(vec![mock_backend_ref.clone()]),
                    timeout_ms: None,
                }],
                attached_level: crate::plugins::filters::SgAttachedLevel::Gateway,
            })
            .await
            .unwrap();
        let mock_backend = SgBackendInst {
            name_or_host: mock_backend_ref.name_or_host,
            namespace: mock_backend_ref.namespace,
            port: mock_backend_ref.port,
            timeout_ms: mock_backend_ref.timeout_ms,
            protocol: mock_backend_ref.protocol,
            weight: mock_backend_ref.weight,
            filters: vec![],
        };
        let mut ctx = SgRoutePluginContext::new_http(
            Method::POST,
            Uri::from_static("http://sg.idealworld.group/iam/ct/001?name=sg"),
            Version::HTTP_11,
            HeaderMap::new(),
            Body::empty(),
            "127.0.0.1:8080".parse().unwrap(),
            gateway_name.clone(),
            Some(ChosenHttpRouteRuleInst::clone_from(&SgHttpRouteRuleInst { ..Default::default() }, None)),
        );

        ctx.set_chose_backend(&mock_backend);

        let ctx = ctx.resp_from_error(TardisError::bad_request("", ""));
        let (is_ok, ctx) = stats.resp_filter("id1", ctx).await.unwrap();
        assert!(is_ok);
        assert_eq!(
            get_status(&mock_backend.name_or_host, &get_cache_key(&stats.cache_key, &ctx.get_gateway_name()), ctx.cache().unwrap()).await.unwrap().unwrap(),
            Status::Minor
        );

        let (_, ctx) = stats.resp_filter("id2", ctx).await.unwrap();
        let (_, ctx) = stats.resp_filter("id3", ctx).await.unwrap();
        let (_, ctx) = stats.resp_filter("id4", ctx).await.unwrap();
        assert_eq!(
            get_status(&mock_backend.name_or_host, &get_cache_key(&stats.cache_key, &ctx.get_gateway_name()), ctx.cache().unwrap()).await.unwrap().unwrap(),
            Status::Major
        );

        let ctx = ctx.resp(StatusCode::OK, HeaderMap::new(), Body::empty());
        let (_, ctx) = stats.resp_filter("id4", ctx).await.unwrap();
        assert_eq!(
            get_status(&mock_backend.name_or_host, &get_cache_key(&stats.cache_key, &ctx.get_gateway_name()), ctx.cache().unwrap()).await.unwrap().unwrap(),
            Status::Good
        );
    }

    pub struct LifeHold<'a> {
        pub redis: Container<'a, Redis>,
    }

    async fn docker_init(docker: &Cli) -> TardisResult<LifeHold<'_>> {
        let redis_container = TardisTestContainer::redis_custom(docker);
        let port = redis_container.get_host_port_ipv4(6379);
        let url = format!("redis://127.0.0.1:{port}/0",);
        env::set_var("TARDIS_FW.CACHE.URL", url);

        Ok(LifeHold { redis: redis_container })
    }
}
