use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(feature = "mwal_backend")]
use std::sync::{Arc, Mutex};

use anyhow::Result;
use database::libsql::LibSqlDb;
use database::service::DbFactoryService;
use database::write_proxy::WriteProxyDbFactory;
use rpc_server::run_proxy_server;

use crate::postgres::service::PgConnectionFactory;
use crate::server::Server;

mod database;
mod libsql;
mod postgres;
mod query;
mod query_analysis;
mod rpc_server;
mod server;

pub mod proxy_rpc {
    #![allow(clippy::all)]
    tonic::include_proto!("proxy");
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum Backend {
    Libsql,
    #[cfg(feature = "mwal_backend")]
    Mwal,
}

pub async fn run_server(
    db_path: PathBuf,
    tcp_addr: SocketAddr,
    ws_addr: Option<SocketAddr>,
    backend: Backend,
    #[cfg(feature = "mwal_backend")] mwal_addr: Option<String>,
    writer_rpc_addr: Option<String>,
    rpc_server_addr: Option<SocketAddr>,
) -> Result<()> {
    let mut server = Server::new();
    server.bind_tcp(tcp_addr).await?;

    if let Some(addr) = ws_addr {
        server.bind_ws(addr).await?;
    }

    tracing::trace!("Backend: {:?}", backend);
    #[cfg(feature = "mwal_backend")]
    if backend == Backend::Mwal {
        std::env::set_var("MVSQLITE_DATA_PLANE", mwal_addr.as_ref().unwrap());
    }

    #[cfg(feature = "mwal_backend")]
    let vwal_methods =
        mwal_addr.map(|_| Arc::new(Mutex::new(mwal::ffi::libsql_wal_methods::new())));

    match writer_rpc_addr {
        Some(addr) => {
            let factory = WriteProxyDbFactory::new(
                addr,
                db_path,
                #[cfg(feature = "mwal_backend")]
                vwal_methods,
            )
            .await?;
            let service = DbFactoryService::new(factory);
            let factory = PgConnectionFactory::new(service);
            server.serve(factory).await;
        }
        None => {
            let db_factory = move || {
                let db_path = db_path.clone();
                #[cfg(feature = "mwal_backend")]
                let vwal_methods = vwal_methods.clone();
                async move {
                    LibSqlDb::new(
                        db_path,
                        #[cfg(feature = "mwal_backend")]
                        vwal_methods,
                        (),
                    )
                }
            };
            let service = DbFactoryService::new(db_factory.clone());
            let factory = PgConnectionFactory::new(service);
            if let Some(addr) = rpc_server_addr {
                tokio::spawn(run_proxy_server(addr, db_factory));
            }
            server.serve(factory).await;
        }
    }

    Ok(())
}
