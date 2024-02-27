//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::net::IpAddr;
use std::str::FromStr;
use std::string::ToString;
use std::sync::Arc;
use std::time::Duration;

use crate::env::{WS_KEEP_ALIVE_INTERVAL, WS_MAX_IDLE_TIME};
use ::http::uri::PathAndQuery;
use ::http::Uri;
use async_trait::async_trait;
use boring::ssl::{SslConnector, SslConnectorBuilder, SslMethod};
use futures_util::TryFutureExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_boring::SslStream;

use crate::infra::certs::RootCertificates;
use crate::infra::connection_manager::{
    MultiRouteConnectionManager, SingleRouteThrottlingConnectionManager,
};
use crate::infra::dns::DnsResolver;
use crate::infra::errors::NetError;
use crate::infra::reconnect::ServiceConnector;
use crate::infra::ws::WebSocketConfig;
use crate::utils::first_ok;

pub mod certs;
pub mod connection_manager;
pub mod dns;
pub mod errors;
pub(crate) mod http;
pub(crate) mod reconnect;
pub(crate) mod tokio_executor;
pub(crate) mod tokio_io;
pub mod ws;

const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(200);

/// A collection of commonly used decorators for HTTP requests.
#[derive(Clone, Debug)]
pub enum HttpRequestDecorator {
    /// Adds the following header to the request:
    /// ```text
    /// Authorization: Basic base64(<username>:<password>)
    /// ```
    HeaderAuth(String),
    /// Prefixes the path portion of the request with the given string.
    PathPrefix(&'static str),
    /// Applies generic decoration logic.
    Generic(fn(hyper::http::request::Builder) -> hyper::http::request::Builder),
}

#[derive(Clone, Debug, Default)]
pub struct HttpRequestDecoratorSeq(Vec<HttpRequestDecorator>);

impl From<HttpRequestDecorator> for HttpRequestDecoratorSeq {
    fn from(value: HttpRequestDecorator) -> Self {
        Self(vec![value])
    }
}

/// Contains all information required to establish an HTTP connection to the remote endpoint:
/// - `sni` value to be used in TLS,
/// - `host` value to be used for DNS resolution an in the HTTP requests headers,
/// - `port` to connect to,
/// - `http_request_decorator`, a [HttpRequestDecorator] to apply to all HTTP requests,
/// - `certs`, [RootCertificates] representing trusted certificates,
/// - `dns_resolver`, a [DnsResolver] to use when resolving DNS.
/// This is also applicable to WebSocket connections (in this case, `http_request_decorator` will
/// only be applied to the initial connection upgrade request).
#[derive(Clone, Debug)]
pub struct ConnectionParams {
    pub sni: Arc<str>,
    pub host: Arc<str>,
    pub port: u16,
    pub http_request_decorator: HttpRequestDecoratorSeq,
    pub certs: RootCertificates,
    pub dns_resolver: Arc<DnsResolver>,
}

impl ConnectionParams {
    pub fn new(
        sni: &str,
        host: &str,
        port: u16,
        http_request_decorator: HttpRequestDecoratorSeq,
        certs: RootCertificates,
        dns_resolver: Arc<DnsResolver>,
    ) -> Self {
        Self {
            sni: Arc::from(sni),
            host: Arc::from(host),
            port,
            http_request_decorator,
            certs,
            dns_resolver,
        }
    }

    pub fn with_decorator(mut self, decorator: HttpRequestDecorator) -> Self {
        let HttpRequestDecoratorSeq(decorators) = &mut self.http_request_decorator;
        decorators.push(decorator);
        self
    }

    pub fn with_certs(mut self, certs: RootCertificates) -> Self {
        self.certs = certs;
        self
    }
}

impl HttpRequestDecoratorSeq {
    pub fn decorate_request(
        &self,
        request_builder: hyper::http::request::Builder,
    ) -> hyper::http::request::Builder {
        self.0
            .iter()
            .fold(request_builder, |rb, dec| dec.decorate_request(rb))
    }
}

impl HttpRequestDecorator {
    fn decorate_request(
        &self,
        request_builder: hyper::http::request::Builder,
    ) -> hyper::http::request::Builder {
        match self {
            Self::Generic(decorator) => decorator(request_builder),
            Self::HeaderAuth(auth) => request_builder.header(::http::header::AUTHORIZATION, auth),
            Self::PathPrefix(prefix) => {
                let uri = request_builder.uri_ref().expect("request has URI set");
                let mut parts = (*uri).clone().into_parts();
                let decorated_pq = match parts.path_and_query {
                    Some(pq) => format!("{}{}", prefix, pq.as_str()),
                    None => prefix.to_string(),
                };
                parts.path_and_query = Some(
                    PathAndQuery::from_str(decorated_pq.as_str()).expect("valid path and query"),
                );
                request_builder.uri(Uri::from_parts(parts).expect("valid uri"))
            }
        }
    }
}

pub struct StreamAndHost<T>(T, url::Host);

pub trait AsyncDuplexStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync> AsyncDuplexStream for S {}

#[async_trait]
pub trait TransportConnector: Clone + Send + Sync {
    type Stream: AsyncDuplexStream + 'static;

    async fn connect(
        &self,
        connection_params: &ConnectionParams,
        alpn: &[u8],
    ) -> Result<StreamAndHost<Self::Stream>, NetError>;
}

#[derive(Clone)]
pub struct TcpSslTransportConnector;

#[async_trait]
impl TransportConnector for TcpSslTransportConnector {
    type Stream = SslStream<TcpStream>;

    async fn connect(
        &self,
        connection_params: &ConnectionParams,
        alpn: &[u8],
    ) -> Result<StreamAndHost<Self::Stream>, NetError> {
        let StreamAndHost(tcp_stream, remote_address) = connect_tcp(
            &connection_params.dns_resolver,
            &connection_params.sni,
            connection_params.port,
        )
        .await?;

        let ssl_config = Self::builder(connection_params.certs, alpn)?
            .build()
            .configure()?;

        let ssl_stream = tokio_boring::connect(ssl_config, &connection_params.sni, tcp_stream)
            .await
            .map_err(|_| NetError::SslFailedHandshake)?;

        Ok(StreamAndHost(ssl_stream, remote_address))
    }
}

impl TcpSslTransportConnector {
    fn builder(certs: RootCertificates, alpn: &[u8]) -> Result<SslConnectorBuilder, NetError> {
        let mut ssl = SslConnector::builder(SslMethod::tls_client())?;
        ssl.set_verify_cert_store(certs.try_into()?)?;
        ssl.set_alpn_protos(alpn)?;
        Ok(ssl)
    }
}

pub struct EndpointConnection<C, S> {
    pub manager: C,
    pub connector: S,
}

impl<S: ServiceConnector> EndpointConnection<MultiRouteConnectionManager, S> {
    pub fn new_multi(
        connection_params: impl IntoIterator<Item = ConnectionParams>,
        connect_timeout: Duration,
        service_connector: S,
    ) -> Self {
        Self {
            manager: MultiRouteConnectionManager::new(
                connection_params
                    .into_iter()
                    .map(|params| {
                        SingleRouteThrottlingConnectionManager::new(params, connect_timeout)
                    })
                    .collect(),
                connect_timeout,
            ),
            connector: service_connector,
        }
    }
}

pub fn make_ws_config(
    websocket_endpoint: PathAndQuery,
    connect_timeout: Duration,
) -> WebSocketConfig {
    WebSocketConfig {
        ws_config: tungstenite::protocol::WebSocketConfig::default(),
        endpoint: websocket_endpoint,
        max_connection_time: connect_timeout,
        keep_alive_interval: WS_KEEP_ALIVE_INTERVAL,
        max_idle_time: WS_MAX_IDLE_TIME,
    }
}

pub(crate) async fn connect_tcp(
    dns_resolver: &DnsResolver,
    host: &str,
    port: u16,
) -> Result<StreamAndHost<TcpStream>, NetError> {
    let dns_lookup = dns_resolver
        .lookup_ip(host)
        .await
        .map_err(|_| NetError::DnsError)?;

    if dns_lookup.is_empty() {
        return Err(NetError::DnsError);
    }

    // The idea is to go through the list of candidate IP addresses
    // and to attempt a connection to each of them, giving each one a `CONNECTION_ATTEMPT_DELAY` headstart
    // before moving on to the next candidate.
    // The process stops once we have a successful connection.

    // First, for each resolved IP address, constructing a future
    // that incorporates the delay based on its position in the list.
    // This way we can start all futures at once and simply wait for the first one to complete successfully.
    let staggered_futures = dns_lookup.into_iter().enumerate().map(|(idx, ip)| {
        let delay = CONNECTION_ATTEMPT_DELAY * idx.try_into().unwrap();
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            TcpStream::connect((ip, port))
                .inspect_err(|e| {
                    log::debug!("failed to connect to IP [{}] with an error: {:?}", ip, e)
                })
                .await
                .map(|r| StreamAndHost(r, ip_addr_to_host(ip)))
        }
    });

    first_ok(staggered_futures)
        .await
        .ok_or(NetError::TcpConnectionFailed)
}

fn ip_addr_to_host(ip: IpAddr) -> url::Host {
    match ip {
        IpAddr::V4(v4) => url::Host::Ipv4(v4),
        IpAddr::V6(v6) => url::Host::Ipv6(v6),
    }
}

#[cfg(test)]
pub(crate) mod test {
    use hyper::Request;

    use crate::infra::HttpRequestDecorator;
    use crate::utils::basic_authorization;

    pub(crate) mod shared {
        use std::fmt::Debug;
        use std::io;
        use std::sync::Arc;
        use std::time::Duration;

        use async_trait::async_trait;
        use derive_where::derive_where;
        use displaydoc::Display;
        use tokio::io::DuplexStream;
        use warp::{Filter, Reply};

        use crate::infra::connection_manager::ConnectionManager;
        use crate::infra::errors::{LogSafeDisplay, NetError};
        use crate::infra::reconnect::{
            ServiceConnector, ServiceInitializer, ServiceState, ServiceStatus,
        };
        use crate::infra::{ConnectionParams, StreamAndHost, TransportConnector};

        #[derive(Debug, Display)]
        pub(crate) enum TestError {
            /// expected error
            Expected,
            /// unexpected error
            Unexpected(&'static str),
        }

        impl LogSafeDisplay for TestError {}

        // the choice of the constant value is dictated by a vague notion of being
        // "not too many, but also not just once or twice"
        pub(crate) const FEW_ATTEMPTS: u16 = 3;

        pub(crate) const MANY_ATTEMPTS: u16 = 1000;

        pub(crate) const TIMEOUT_DURATION: Duration = Duration::from_millis(100);

        pub(crate) const NORMAL_CONNECTION_TIME: Duration = Duration::from_millis(20);

        pub(crate) const LONG_CONNECTION_TIME: Duration = Duration::from_secs(10);

        // we need to advance time in tests by some value not to run into the scenario
        // of attempts starting at the same time, but also by not too much so that we
        // don't step over the cool down time
        pub(crate) const TIME_ADVANCE_VALUE: Duration = Duration::from_millis(5);

        #[derive(Clone)]
        pub(crate) struct InMemoryWarpConnector<F> {
            filter: F,
        }

        impl<F> InMemoryWarpConnector<F> {
            pub fn new(filter: F) -> Self {
                Self { filter }
            }
        }

        #[async_trait]
        impl<F> TransportConnector for InMemoryWarpConnector<F>
        where
            F: Filter + Clone + Send + Sync + 'static,
            F::Extract: Reply,
        {
            type Stream = DuplexStream;

            async fn connect(
                &self,
                connection_params: &ConnectionParams,
                _alpn: &[u8],
            ) -> Result<StreamAndHost<Self::Stream>, NetError> {
                let (client, server) = tokio::io::duplex(1024);
                let routes = self.filter.clone();
                tokio::spawn(async {
                    let one_element_iter =
                        futures_util::stream::iter(vec![Ok::<DuplexStream, io::Error>(server)]);
                    warp::serve(routes).run_incoming(one_element_iter).await;
                });
                Ok(StreamAndHost(
                    client,
                    url::Host::Domain(connection_params.host.to_string()),
                ))
            }
        }

        #[derive_where(Clone)]
        pub struct NoReconnectService<C: ServiceConnector> {
            pub(crate) inner: Arc<ServiceState<C::Service, C::Error>>,
        }

        impl<C> NoReconnectService<C>
        where
            C: ServiceConnector + Send + Sync + 'static,
            C::Service: Clone + Send + Sync + 'static,
            C::Channel: Send + Sync,
            C::Error: Send + Sync + Debug + LogSafeDisplay,
        {
            pub async fn start<M>(service_connector: C, connection_manager: M) -> Self
            where
                M: ConnectionManager + 'static,
            {
                let status = ServiceInitializer::new(service_connector, connection_manager)
                    .connect()
                    .await;
                Self {
                    inner: Arc::new(status),
                }
            }

            pub fn service_status(&self) -> Option<&ServiceStatus<C::Error>> {
                match &*self.inner {
                    ServiceState::Active(_, status) => Some(status),
                    _ => None,
                }
            }
        }
    }

    #[test]
    fn test_path_prefix_decorator() {
        let cases = vec![
            ("https://chat.signal.org/", "/chat/"),
            ("https://chat.signal.org/v1", "/chat/v1"),
            ("https://chat.signal.org/v1?a=b", "/chat/v1"),
            ("https://chat.signal.org/v1/endpoint", "/chat/v1/endpoint"),
        ];
        for (input, expected_path) in cases.into_iter() {
            let builder = Request::get(input);
            let builder = HttpRequestDecorator::PathPrefix("/chat").decorate_request(builder);
            let (parts, _) = builder.body(()).unwrap().into_parts();
            assert_eq!(expected_path, parts.uri.path(), "for input [{}]", input)
        }
    }

    #[test]
    fn test_header_auth_decorator() {
        let expected = "Basic dXNybm06cHNzd2Q=";
        let builder = Request::get("https://chat.signal.org/");
        let builder = HttpRequestDecorator::HeaderAuth(basic_authorization("usrnm", "psswd"))
            .decorate_request(builder);
        let (parts, _) = builder.body(()).unwrap().into_parts();
        assert_eq!(
            expected,
            parts.headers.get(http::header::AUTHORIZATION).unwrap()
        );
    }
}
