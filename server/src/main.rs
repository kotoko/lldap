#![forbid(unsafe_code)]
#![forbid(non_ascii_idents)]
#![allow(clippy::nonstandard_macro_braces)]

use crate::{
    domain::{
        handler::{BackendHandler, CreateUserRequest, GroupRequestFilter},
        sql_backend_handler::SqlBackendHandler,
        sql_opaque_handler::register_password,
        sql_tables::PoolOptions,
    },
    infra::{cli::*, configuration::Configuration, db_cleaner::Scheduler, mail},
};
use actix::Actor;
use actix_server::ServerBuilder;
use anyhow::{anyhow, Context, Result};
use futures_util::TryFutureExt;
use tracing::*;

mod domain;
mod infra;

async fn create_admin_user(handler: &SqlBackendHandler, config: &Configuration) -> Result<()> {
    let pass_length = config.ldap_user_pass.unsecure().len();
    assert!(
        pass_length >= 6,
        "Minimum password length is 6 characters, got {} characters",
        pass_length
    );
    handler
        .create_user(CreateUserRequest {
            user_id: config.ldap_user_dn.clone(),
            email: config.ldap_user_email.clone(),
            display_name: Some("Administrator".to_string()),
            ..Default::default()
        })
        .and_then(|_| register_password(handler, &config.ldap_user_dn, &config.ldap_user_pass))
        .await
        .context("Error creating admin user")?;
    let admin_group_id = handler
        .create_group("lldap_admin")
        .await
        .context("Error creating admin group")?;
    handler
        .add_user_to_group(&config.ldap_user_dn, admin_group_id)
        .await
        .context("Error adding admin user to group")
}

#[instrument(skip_all)]
async fn set_up_server(config: Configuration) -> Result<ServerBuilder> {
    info!("Starting LLDAP version {}", env!("CARGO_PKG_VERSION"));

    let sql_pool = PoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .context("while connecting to the DB")?;
    domain::sql_tables::init_table(&sql_pool)
        .await
        .context("while creating the tables")?;
    let backend_handler = SqlBackendHandler::new(config.clone(), sql_pool.clone());
    if let Err(e) = backend_handler.get_user_details(&config.ldap_user_dn).await {
        warn!("Could not get admin user, trying to create it: {:#}", e);
        create_admin_user(&backend_handler, &config)
            .await
            .map_err(|e| anyhow!("Error setting up admin login/account: {:#}", e))
            .context("while creating the admin user")?;
    }
    if backend_handler
        .list_groups(Some(GroupRequestFilter::DisplayName(
            "lldap_password_manager".to_string(),
        )))
        .await?
        .is_empty()
    {
        warn!("Could not find password_manager group, trying to create it");
        backend_handler
            .create_group("lldap_password_manager")
            .await
            .context("while creating password_manager group")?;
        backend_handler
            .create_group("lldap_strict_readonly")
            .await
            .context("while creating readonly group")?;
    }
    let server_builder = infra::ldap_server::build_ldap_server(
        &config,
        backend_handler.clone(),
        actix_server::Server::build(),
    )
    .context("while binding the LDAP server")?;
    infra::jwt_sql_tables::init_table(&sql_pool).await?;
    let server_builder =
        infra::tcp_server::build_tcp_server(&config, backend_handler, server_builder)
            .await
            .context("while binding the TCP server")?;
    // Run every hour.
    let scheduler = Scheduler::new("0 0 * * * * *", sql_pool);
    scheduler.start();
    Ok(server_builder)
}

async fn run_server(config: Configuration) -> Result<()> {
    set_up_server(config)
        .await?
        .workers(1)
        .run()
        .await
        .context("while starting the server")?;
    Ok(())
}

fn run_server_command(opts: RunOpts) -> Result<()> {
    debug!("CLI: {:#?}", &opts);

    let config = infra::configuration::init(opts)?;
    infra::logging::init(&config)?;

    actix::run(
        run_server(config).unwrap_or_else(|e| error!("Could not bring up the servers: {:#}", e)),
    )?;

    info!("End.");
    Ok(())
}

fn send_test_email_command(opts: TestEmailOpts) -> Result<()> {
    let to = opts.to.parse()?;
    let config = infra::configuration::init(opts)?;
    infra::logging::init(&config)?;
    mail::send_test_email(to, &config.smtp_options)
}

fn main() -> Result<()> {
    let cli_opts = infra::cli::init();
    match cli_opts.command {
        Command::ExportGraphQLSchema(opts) => infra::graphql::api::export_schema(opts),
        Command::Run(opts) => run_server_command(opts),
        Command::SendTestEmail(opts) => send_test_email_command(opts),
    }
}
