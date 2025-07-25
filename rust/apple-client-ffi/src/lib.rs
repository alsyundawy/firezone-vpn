// Swift bridge generated code triggers this below
#![allow(clippy::unnecessary_cast, improper_ctypes, non_camel_case_types)]
#![cfg(unix)]

mod make_writer;
mod tun;

use anyhow::Context;
use anyhow::Result;
use backoff::ExponentialBackoffBuilder;
use client_shared::{DisconnectError, Event, Session, V4RouteList, V6RouteList};
use connlib_model::ResourceView;
use dns_types::DomainName;
use firezone_logging::err_with_src;
use firezone_logging::sentry_layer;
use firezone_telemetry::APPLE_DSN;
use firezone_telemetry::Telemetry;
use firezone_telemetry::analytics;
use ip_network::{Ipv4Network, Ipv6Network};
use phoenix_channel::LoginUrl;
use phoenix_channel::PhoenixChannel;
use phoenix_channel::get_user_agent;
use secrecy::{Secret, SecretString};
use std::sync::OnceLock;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::runtime::Runtime;
use tracing_subscriber::prelude::*;
use tun::Tun;

/// The Apple client implements reconnect logic in the upper layer using OS provided
/// APIs to detect network connectivity changes. The reconnect timeout here only
/// applies only in the following conditions:
///
/// * That reconnect logic fails to detect network changes (not expected to happen)
/// * The portal is DOWN
///
/// Hopefully we aren't down for more than 24 hours.
const MAX_PARTITION_TIME: Duration = Duration::from_secs(60 * 60 * 24);

/// The Sentry release.
///
/// This module is only responsible for the connlib part of the MacOS/iOS app.
/// Bugs within the MacOS/iOS app itself may use the same DSN but a different component as part of the version string.
const RELEASE: &str = concat!("connlib-apple@", env!("CARGO_PKG_VERSION"));

#[swift_bridge::bridge]
mod ffi {
    extern "Rust" {
        type WrappedSession;
        type DisconnectError;

        #[swift_bridge(associated_to = WrappedSession, return_with = err_to_string)]
        fn connect(
            api_url: String,
            token: String,
            device_id: String,
            account_slug: String,
            device_name_override: Option<String>,
            os_version_override: Option<String>,
            log_dir: String,
            log_filter: String,
            callback_handler: CallbackHandler,
            device_info: String,
        ) -> Result<WrappedSession, String>;

        fn reset(self: &mut WrappedSession, reason: String);

        // Set system DNS resolvers
        //
        // `dns_servers` must not have any IPv6 scopes
        // <https://github.com/firezone/firezone/issues/4350>
        #[swift_bridge(swift_name = "setDns", return_with = err_to_string)]
        fn set_dns(self: &mut WrappedSession, dns_servers: String) -> Result<(), String>;

        #[swift_bridge(swift_name = "setDisabledResources", return_with = err_to_string)]
        fn set_disabled_resources(
            self: &mut WrappedSession,
            disabled_resources: String,
        ) -> Result<(), String>;

        #[swift_bridge(swift_name = "setLogDirectives", return_with = err_to_string)]
        fn set_log_directives(self: &mut WrappedSession, directives: String) -> Result<(), String>;

        #[swift_bridge(swift_name = "isAuthenticationError")]
        fn is_authentication_error(self: &DisconnectError) -> bool;

        #[swift_bridge(swift_name = "toString")]
        fn to_string(self: &DisconnectError) -> String;
    }

    extern "Swift" {
        type CallbackHandler;

        #[swift_bridge(swift_name = "onSetInterfaceConfig")]
        fn on_set_interface_config(
            &self,
            tunnelAddressIPv4: String,
            tunnelAddressIPv6: String,
            searchDomain: Option<String>,
            dnsAddresses: String,
            routeListv4: String,
            routeListv6: String,
        );

        #[swift_bridge(swift_name = "onUpdateResources")]
        fn on_update_resources(&self, resourceList: String);

        #[swift_bridge(swift_name = "onDisconnect")]
        fn on_disconnect(&self, error: DisconnectError);
    }
}

/// This is used by the apple client to interact with our code.
pub struct WrappedSession {
    inner: Session,
    runtime: Runtime,

    telemetry: Telemetry,
}

// SAFETY: `CallbackHandler.swift` promises to be thread-safe.
// TODO: Uphold that promise!
unsafe impl Send for ffi::CallbackHandler {}
unsafe impl Sync for ffi::CallbackHandler {}

pub struct CallbackHandler {
    // Generated Swift opaque type wrappers have a `Drop` impl that decrements the
    // refcount, but there's no way to generate a `Clone` impl that increments the
    // recount. Instead, we just wrap it in an `Arc`.
    inner: ffi::CallbackHandler,
}

impl CallbackHandler {
    fn on_set_interface_config(
        &self,
        tunnel_address_v4: Ipv4Addr,
        tunnel_address_v6: Ipv6Addr,
        dns_addresses: Vec<IpAddr>,
        search_domain: Option<DomainName>,
        route_list_v4: Vec<Ipv4Network>,
        route_list_v6: Vec<Ipv6Network>,
    ) {
        match (
            serde_json::to_string(&dns_addresses),
            serde_json::to_string(&V4RouteList::new(route_list_v4)),
            serde_json::to_string(&V6RouteList::new(route_list_v6)),
        ) {
            (Ok(dns_addresses), Ok(route_list_4), Ok(route_list_6)) => {
                self.inner.on_set_interface_config(
                    tunnel_address_v4.to_string(),
                    tunnel_address_v6.to_string(),
                    search_domain.map(|s| s.to_string()),
                    dns_addresses,
                    route_list_4,
                    route_list_6,
                );
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                tracing::error!("Failed to serialize to JSON: {}", err_with_src(&e));
            }
        }
    }

    fn on_update_resources(&self, resource_list: Vec<ResourceView>) {
        let resource_list = match serde_json::to_string(&resource_list) {
            Ok(resource_list) => resource_list,
            Err(e) => {
                tracing::error!("Failed to serialize resource list: {}", err_with_src(&e));
                return;
            }
        };

        self.inner.on_update_resources(resource_list);
    }

    fn on_disconnect(&self, error: DisconnectError) {
        if !error.is_authentication_error() {
            tracing::error!("{error}")
        }

        self.inner.on_disconnect(error);
    }
}

static LOGGER_STATE: OnceLock<(
    firezone_logging::file::Handle,
    firezone_logging::FilterReloadHandle,
)> = OnceLock::new();

/// Initialises a global logger with the specified log filter.
///
/// A global logger can only be set once, hence this function uses `static` state to check whether a logger has already been set.
/// If so, the new `log_filter` will be applied to the existing logger but a different `log_dir` won't have any effect.
///
/// From within the FFI module, we have no control over our memory lifecycle and we may get initialised multiple times within the same process.
fn init_logging(log_dir: PathBuf, log_filter: String) -> Result<()> {
    if let Some((_, reload_handle)) = LOGGER_STATE.get() {
        reload_handle
            .reload(&log_filter)
            .context("Failed to apply new log-filter")?;

        return Ok(());
    }

    let (file_log_filter, file_reload_handle) = firezone_logging::try_filter(&log_filter)?;
    let (oslog_log_filter, oslog_reload_handle) = firezone_logging::try_filter(&log_filter)?;

    let (file_layer, handle) = firezone_logging::file::layer(&log_dir, "connlib");

    let subscriber = tracing_subscriber::registry()
        .with(file_layer.with_filter(file_log_filter))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .event_format(
                    firezone_logging::Format::new()
                        .without_timestamp()
                        .without_level(),
                )
                .with_writer(make_writer::MakeWriter::new(
                    "dev.firezone.firezone",
                    "connlib",
                ))
                .with_filter(oslog_log_filter),
        )
        .with(sentry_layer());

    let reload_handle = file_reload_handle.merge(oslog_reload_handle);

    firezone_logging::init(subscriber)?;

    LOGGER_STATE
        .set((handle, reload_handle))
        .expect("logger state should only ever be initialised once");

    Ok(())
}

impl WrappedSession {
    // TODO: Refactor this when we refactor PhoenixChannel.
    // See https://github.com/firezone/firezone/issues/2158
    #[expect(clippy::too_many_arguments)]
    fn connect(
        api_url: String,
        token: String,
        device_id: String,
        account_slug: String,
        device_name_override: Option<String>,
        os_version_override: Option<String>,
        log_dir: String,
        log_filter: String,
        callback_handler: ffi::CallbackHandler,
        device_info: String,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("connlib")
            .enable_all()
            .build()?;

        let mut telemetry = Telemetry::default();
        runtime.block_on(telemetry.start(&api_url, RELEASE, APPLE_DSN, device_id.clone()));
        Telemetry::set_account_slug(account_slug.clone());

        analytics::identify(RELEASE.to_owned(), Some(account_slug));

        init_logging(log_dir.into(), log_filter)?;
        install_rustls_crypto_provider();

        let secret = SecretString::from(token);
        let device_info =
            serde_json::from_str(&device_info).context("Failed to deserialize `DeviceInfo`")?;

        let url = LoginUrl::client(
            api_url.as_str(),
            &secret,
            device_id.clone(),
            device_name_override,
            device_info,
        )?;

        let _guard = runtime.enter(); // Constructing `PhoenixChannel` requires a runtime context.

        let portal = PhoenixChannel::disconnected(
            Secret::new(url),
            get_user_agent(os_version_override, env!("CARGO_PKG_VERSION")),
            "client",
            (),
            || {
                ExponentialBackoffBuilder::default()
                    .with_max_elapsed_time(Some(MAX_PARTITION_TIME))
                    .build()
            },
            Arc::new(socket_factory::tcp),
        )?;
        let (session, mut event_stream) = Session::connect(
            Arc::new(socket_factory::tcp),
            Arc::new(socket_factory::udp),
            portal,
            runtime.handle().clone(),
        );
        session.set_tun(Box::new(Tun::new()?));

        analytics::new_session(device_id, api_url.to_string());

        runtime.spawn(async move {
            let callback_handler = CallbackHandler {
                inner: callback_handler,
            };

            while let Some(event) = event_stream.next().await {
                match event {
                    Event::TunInterfaceUpdated {
                        ipv4,
                        ipv6,
                        dns,
                        search_domain,
                        ipv4_routes,
                        ipv6_routes,
                    } => {
                        callback_handler.on_set_interface_config(
                            ipv4,
                            ipv6,
                            dns,
                            search_domain,
                            ipv4_routes,
                            ipv6_routes,
                        );
                    }
                    Event::ResourcesUpdated(resource_views) => {
                        callback_handler.on_update_resources(resource_views);
                    }
                    Event::Disconnected(error) => {
                        callback_handler.on_disconnect(error);
                    }
                }
            }
        });

        Ok(Self {
            inner: session,
            runtime,
            telemetry,
        })
    }

    fn reset(&mut self, reason: String) {
        self.inner.reset(reason)
    }

    fn set_dns(&mut self, dns_servers: String) -> Result<()> {
        tracing::debug!(%dns_servers);

        let dns_servers = serde_json::from_str(&dns_servers)
            .context("Failed to deserialize DNS servers from JSON")?;

        self.inner.set_dns(dns_servers);

        Ok(())
    }

    fn set_disabled_resources(&mut self, disabled_resources: String) -> Result<()> {
        tracing::debug!(%disabled_resources);

        let disabled_resources = serde_json::from_str(&disabled_resources)
            .context("Failed to deserialize disabled resources from JSON")?;

        self.inner.set_disabled_resources(disabled_resources);

        Ok(())
    }

    fn set_log_directives(&mut self, directives: String) -> Result<()> {
        let (_, handle) = LOGGER_STATE.get().context("Logger is not initialised")?;

        handle.reload(&directives)?;

        Ok(())
    }
}

impl Drop for WrappedSession {
    fn drop(&mut self) {
        self.runtime.block_on(self.telemetry.stop());
    }
}

fn err_to_string<T>(result: Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("{e:#}"))
}

/// Installs the `ring` crypto provider for rustls.
fn install_rustls_crypto_provider() {
    let existing = rustls::crypto::ring::default_provider().install_default();

    if existing.is_err() {
        tracing::debug!("Skipping install of crypto provider because we already have one.");
    }
}
