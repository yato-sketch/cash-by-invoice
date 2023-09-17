use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cashu::Cashu;
use cashu_sdk::{Amount, Bolt11Invoice};
use clap::Parser;
use cln_rpc::model::{
    requests::{InvoiceRequest, PayRequest, WaitanyinvoiceRequest},
    responses::WaitanyinvoiceResponse,
};
use cln_rpc::primitives::{Amount as CLN_Amount, AmountOrAny};
use cln_rpc::ClnRpc;
use database::Db;
use dirs::data_dir;
use futures::{Stream, StreamExt};
use nostr_sdk::secp256k1::XOnlyPublicKey;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use types::{as_msat, unix_time, PendingInvoice, User};
use url::Url;
use uuid::Uuid;

use crate::cli::CLIArgs;
use crate::config::{Info, Network, Settings};
use crate::nostr::Nostr;

mod cashu;
mod cli;
mod config;
mod database;
mod error;
mod nostr;
mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let args = CLIArgs::parse();

    let config_file_settings = match args.config {
        Some(config_path) => config::Settings::new(&Some(config_path)),
        None => Settings::default(),
    };

    let url = match args.url {
        Some(url) => url,
        None => config_file_settings.info.url,
    };

    let mint = args.mint.unwrap_or(config_file_settings.info.mint);

    let invoice_description = args
        .invoice_description
        .or(config_file_settings.info.invoice_description);

    let nostr_nsec = match args.nsec {
        Some(nsec) => Some(nsec),
        None => config_file_settings.info.nostr_nsec,
    };

    let relays = if args.relays.is_empty() {
        config_file_settings.info.relays
    } else {
        args.relays.into_iter().collect()
    };

    let max_sendable: Amount = args.max_sendable.map(|m| Amount::from_sat(m)).unwrap_or(
        config_file_settings
            .info
            .max_sendable
            .map(|a| a)
            .unwrap_or(Amount::from_sat(1000000)),
    );

    let min_sendable: Amount = args.max_sendable.map(|m| Amount::from_sat(m)).unwrap_or(
        config_file_settings
            .info
            .min_sendable
            .map(|a| a)
            .unwrap_or(Amount::from_sat(1)),
    );

    let db_path = args.db_path.or(config_file_settings.info.db_path);

    let proxy = args.proxy.unwrap_or(config_file_settings.info.proxy);

    let cln_path = args.cln_path.or(config_file_settings.info.cln_path);

    let zapper = Some(
        args.zapper
            .unwrap_or(config_file_settings.info.zapper.unwrap_or_default()),
    );

    let pay_index_path = args
        .pay_index_path
        .or(config_file_settings.info.pay_index_path);

    let address = args.address.unwrap_or(config_file_settings.network.address);

    let port = args.port.unwrap_or(config_file_settings.network.port);

    let settings = Settings {
        info: Info {
            url,
            nostr_nsec,
            relays,
            mint,
            invoice_description,
            proxy,
            cln_path,
            min_sendable: Some(min_sendable),
            max_sendable: Some(max_sendable),
            zapper,
            db_path,
            pay_index_path,
        },
        network: Network { port, address },
    };

    let api_base_address = Url::from_str(&settings.info.url)?;
    let description = match settings.info.invoice_description {
        Some(des) => des,
        None => "Hello World".to_string(),
    };
    let nostr_nsec = settings.info.nostr_nsec;
    let relays = settings.info.relays;

    debug!("Relays: {:?}", relays);

    if relays.is_empty() {
        bail!("Must define at least one relay");
    }

    let db_path = match settings.info.db_path {
        Some(path) => PathBuf::from_str(&path)?,
        None => {
            let data_dir = dirs::data_dir().ok_or(anyhow!("Could not get data dir".to_string()))?;
            data_dir.join("cashu-lnurl")
        }
    };

    let db = Db::new(db_path).await?;

    let nostr = Nostr::new(
        db.clone(),
        api_base_address.to_string(),
        &nostr_nsec,
        relays,
    )
    .await?;

    let cashu = Cashu::new(db.clone(), nostr.clone());

    let mut nostr_clone = nostr.clone();
    let nostr_task = tokio::spawn(async move { nostr_clone.run().await });

    let cashu_clone = cashu.clone();
    let cashu_task = tokio::spawn(async move { cashu_clone.run().await });

    let cln_client = if let Some(cln_path) = settings.info.cln_path.clone() {
        Arc::new(Mutex::new(Some(ClnRpc::new(cln_path).await?)))
    } else {
        Arc::new(Mutex::new(None))
    };

    let db_clone = db.clone();
    let cashu_clone = cashu.clone();
    let cln_client_clone = cln_client.clone();

    let state = LnurlState {
        api_base_address,
        min_sendable,
        max_sendable,
        description,
        nostr_pubkey: Some(nostr.get_pubkey()),
        proxy: settings.info.proxy,
        cashu,
        db,
        cln_client,
        nostr,
    };

    let lnurl_service = Router::new()
        .route("/.well-known/lnurlp/:username", get(get_user_lnurl_struct))
        .route("/lnurlp/:username/invoice", get(get_user_invoice))
        .route("/signup", get(get_sign_up))
        .with_state(state);

    let address = settings.network.address;
    let ip = Ipv4Addr::from_str(&address)?;

    let port = settings.network.port;

    let listen_addr = SocketAddr::new(std::net::IpAddr::V4(ip), port);

    let axum_task = axum::Server::bind(&listen_addr).serve(lnurl_service.into_make_service());

    // Task that waits for invoice to be paid
    // When an invoice paid check db if invoice exists request mint and pay and mint
    // DM tokens to user

    if settings.info.proxy {
        let rpc_socket = settings
            .info
            .cln_path
            .clone()
            .expect("CLN RPC socket path required");

        let wait_invoice_task = tokio::spawn(async move {
            let pay_index_path = match settings.info.pay_index_path {
                Some(path) => path,
                None => index_file_path().expect("Could not get path to pay index file"),
            };

            let last_pay_index = match read_last_pay_index(&pay_index_path) {
                Ok(idx) => idx,
                Err(e) => {
                    warn!("Could not read last pay index: {e}");
                    if let Err(e) = write_last_pay_index(&pay_index_path, 0) {
                        warn!("Write error: {e}");
                    }
                    0
                }
            };
            info!("Starting at pay index: {last_pay_index}");

            let mut invoices = invoice_stream(&rpc_socket, pay_index_path, Some(last_pay_index))
                .await
                .unwrap();
            let db = db_clone;
            let cashu = cashu_clone;
            let cln_client = cln_client_clone;

            while let Some((hash, _invoice)) = invoices.next().await {
                // Check if invoice is in db and proxied
                // If it is request mint from selected mint
                if let Ok(Some(invoice)) = db.get_pending_invoice(&hash).await {
                    // Fee to account for routing fee
                    let fee =
                        Amount::from_sat((invoice.amount.to_sat() as f32 * 0.01).ceil() as u64);

                    let amount = invoice.amount - fee;

                    let request_mint_response =
                        match cashu.request_mint(amount, &invoice.mint).await {
                            Ok(res) => res,
                            Err(err) => {
                                warn!("{:?}", err);
                                continue;
                            }
                        };

                    let pending_invoice = PendingInvoice {
                        mint: invoice.mint,
                        username: invoice.username,
                        description: invoice.description,
                        amount,
                        hash: request_mint_response.hash,
                        bolt11: request_mint_response.pr.clone(),
                        last_checked: None,
                        proxied: true,
                        time: unix_time(),
                    };

                    // Add mint pending ivoice to DB
                    if let Err(err) = cashu.add_pending_invoice(&pending_invoice).await {
                        warn!("Could not add pending invoice: {:?}", err)
                    }

                    // Remove paid invoice from pending
                    if let Err(err) = db.remove_pending_invoice(&invoice.hash).await {
                        warn!("Could not remove pending invoice {:?}", err);
                    }

                    // Pay mint invoice
                    let mut cln_client = cln_client.lock().await;

                    let cln_response = cln_client
                        .as_mut()
                        .unwrap()
                        .call(cln_rpc::Request::Pay(PayRequest {
                            bolt11: request_mint_response.pr.to_string(),
                            amount_msat: None,
                            label: None,
                            riskfactor: None,
                            maxfeepercent: None,
                            retry_for: None,
                            maxdelay: None,
                            exemptfee: None,
                            localinvreqid: None,
                            exclude: None,
                            maxfee: Some(CLN_Amount::from_sat(fee.to_sat())),
                            description: None,
                        }))
                        .await;

                    match cln_response {
                        Ok(cln_rpc::Response::Pay(pay_response)) => {
                            if let Ok(pay_response) =
                                serde_json::to_string(&pay_response.payment_preimage)
                            {
                                // let invoice = Amount::from_msat(pay_response.amount_sent_msat.msat());
                                debug!("Invoice paid: {:?}", pay_response);
                            }
                        }
                        Ok(res) => warn!("Wrong CLN response: {:?}", res),
                        Err(err) => warn!("Error paying mint invoice: {:?}", err),
                    };
                }
            }
        });

        tokio::select! {
            _ = nostr_task => {
                warn!("Nostr task ended");
            }
            _ = cashu_task => {
                warn!("Cashu task ended");
            }
            _ = axum_task => {
                warn!("Axum task ended");
            }
            _ = wait_invoice_task => {
                warn!("Wait invoice task ended");

            }
        }
    } else {
        tokio::select! {
            _ = nostr_task => {
                warn!("Nostr task ended");
            }
            _ = cashu_task => {
                warn!("Cashu task ended");
            }
            _ = axum_task => {
                warn!("Axum task ended");
            }
        }
    }

    Ok(())
}

async fn invoice_stream(
    socket_addr: &str,
    pay_index_path: PathBuf,
    last_pay_index: Option<u64>,
) -> anyhow::Result<impl Stream<Item = (String, WaitanyinvoiceResponse)>> {
    let cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    Ok(futures::stream::unfold(
        (cln_client, pay_index_path, last_pay_index),
        |(mut cln_client, pay_index_path, mut last_pay_idx)| async move {
            // We loop here since some invoices aren't zaps, in which case we wait for the next one and don't yield
            loop {
                // info!("Waiting for index: {last_pay_idx:?}");
                let invoice_res = cln_client
                    .call(cln_rpc::Request::WaitAnyInvoice(WaitanyinvoiceRequest {
                        timeout: None,
                        lastpay_index: last_pay_idx,
                    }))
                    .await;

                let invoice: WaitanyinvoiceResponse = match invoice_res {
                    Ok(invoice) => invoice,
                    Err(e) => {
                        warn!("Error fetching invoice: {e}");
                        // Let's not spam CLN with requests on failure
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        // Retry same request
                        continue;
                    }
                }
                .try_into()
                .expect("Wrong response from CLN");

                last_pay_idx = invoice.pay_index;
                if let Some(idx) = last_pay_idx {
                    if let Err(e) = write_last_pay_index(&pay_index_path, idx) {
                        warn!("Could not write index tip: {e}");
                    }
                };
                let pay_idx = last_pay_idx;

                break Some((
                    (invoice.payment_hash.to_string(), invoice),
                    (cln_client, pay_index_path, pay_idx),
                ));
            }
        },
    )
    .boxed())
}

/// Default file path for last pay index tip
fn index_file_path() -> anyhow::Result<PathBuf> {
    let mut file_path = match data_dir() {
        Some(path) => path,
        None => return Err(anyhow!("no data dir")),
    };

    file_path.push("cln-zapper");
    file_path.push("last_pay_index");

    Ok(file_path)
}

/// Read last pay index tip from file
fn read_last_pay_index(file_path: &PathBuf) -> anyhow::Result<u64> {
    let mut file = File::open(file_path)?;
    let mut buffer = [0; 8];

    file.read_exact(&mut buffer)?;
    Ok(u64::from_ne_bytes(buffer))
}

/// Write last pay index tip to file
fn write_last_pay_index(file_path: &PathBuf, last_pay_index: u64) -> anyhow::Result<()> {
    // Create the directory if it doesn't exist
    if let Some(parent_dir) = file_path.parent() {
        fs::create_dir_all(parent_dir)?;
    }

    let mut file = File::create(file_path)?;
    file.write_all(&last_pay_index.to_ne_bytes())?;
    Ok(())
}

async fn get_user_lnurl_struct(
    State(state): State<LnurlState>,
    Path(username): Path<String>,
) -> Result<Json<LnurlResponse>, StatusCode> {
    let _user = match state.db.get_user(&username).await {
        Ok(Some(user)) => user,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(err) => {
            warn!("{:?}", err);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let mut callback = state
        .api_base_address
        .join("lnurlp")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    callback
        .path_segments_mut()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .push(&username);
    callback
        .path_segments_mut()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .push("invoice");

    Ok(Json(LnurlResponse {
        min_sendable: state.min_sendable,
        max_sendable: state.max_sendable,
        metadata: serde_json::to_string(&vec![vec!["text/plain".to_string(), state.description]])
            .map_err(|err| {
            warn!("{err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?,
        callback,
        tag: LnurlTag::PayRequest,
        allows_nostr: state.nostr_pubkey.is_some(),
        nostr_pubkey: state.nostr_pubkey,
    }))
}

async fn get_user_invoice(
    Query(params): Query<GetInvoiceParams>,
    Path(username): Path<String>,
    State(state): State<LnurlState>,
) -> Result<Json<GetInvoiceResponse>, StatusCode> {
    let db = state.db;

    let user = match db.get_user(&username).await {
        Ok(Some(user)) => user,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(err) => {
            warn!("{:?}", err);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let mint = &user.mint;
    let amount = Amount::from_msat(params.amount);

    let pending_invoice = if state.proxy && user.proxy {
        let client = state.cln_client.clone();

        let cln_response = client
            .lock()
            .await
            .as_mut()
            .unwrap()
            .call(cln_rpc::Request::Invoice(InvoiceRequest {
                amount_msat: AmountOrAny::Amount(CLN_Amount::from_sat(amount.to_sat())),
                description: params.nostr.clone().unwrap_or_default(),
                label: Uuid::new_v4().to_string(),
                expiry: None,
                fallbacks: None,
                preimage: None,
                cltv: None,
                deschashonly: Some(true),
            }))
            .await;

        match cln_response {
            Ok(cln_rpc::Response::Invoice(invoice_response)) => {
                let invoice = Bolt11Invoice::from_str(&invoice_response.bolt11).unwrap();
                let pending_invoice = PendingInvoice {
                    mint: mint.to_string(),
                    username,
                    description: params.clone().nostr,
                    amount: Amount::from_msat(params.amount),
                    time: unix_time(),
                    hash: invoice_response.payment_hash.to_string(),
                    bolt11: invoice,
                    last_checked: Some(unix_time()),
                    proxied: true,
                };
                state
                    .cashu
                    .add_pending_invoice(&pending_invoice)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                Ok(pending_invoice)
            }
            Ok(res) => {
                warn!("Returned Wrong Cln response: {:?}", res);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Err(err) => {
                error!("CLN RPC error: {:?}", err);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        let request_mint_response =
            state
                .cashu
                .request_mint(amount, mint)
                .await
                .map_err(|err| {
                    warn!("{:?}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        Ok(PendingInvoice {
            mint: mint.to_string(),
            username,
            description: params.nostr,
            amount: Amount::from_msat(params.amount),
            hash: request_mint_response.hash,
            bolt11: request_mint_response.pr,
            last_checked: None,
            proxied: false,
            time: unix_time(),
        })
    };

    match pending_invoice {
        Ok(invoice) => Ok(Json(GetInvoiceResponse {
            pr: invoice.bolt11.to_string(),
            success_action: None,
            routes: vec![],
        })),
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignupParams {
    username: String,
    pubkey: XOnlyPublicKey,
    proxy: Option<bool>,
    mint: String,
    relays: Option<HashSet<String>>,
}

async fn get_sign_up(
    Query(params): Query<SignupParams>,
    State(state): State<LnurlState>,
) -> Result<StatusCode, StatusCode> {
    if let Ok(Some(_)) = state.db.get_user(&params.username).await {
        return Ok(StatusCode::CONFLICT);
    }

    let relays = if let Some(relays) = params.relays {
        relays
    } else {
        HashSet::new()
    };

    let proxy = params.proxy.unwrap_or_default();

    let new_user = User {
        mint: params.mint,
        pubkey: params.pubkey.to_string(),
        relays,
        proxy,
    };

    state
        .db
        .add_user(&params.username, &new_user)
        .await
        .unwrap();

    let nostr = state.nostr.clone();

    let _ = thread::spawn(move || {
        let _ = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(nostr.send_sign_up_message(&params.username, &new_user));
    });

    Ok(StatusCode::OK)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetInvoiceParams {
    amount: u64,
    nostr: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetInvoiceResponse {
    pr: String,
    // TODO: find out proper type
    success_action: Option<String>,
    // TODO: find out proper type
    routes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum LnurlTag {
    PayRequest,
}

#[derive(Clone)]
struct LnurlState {
    api_base_address: Url,
    min_sendable: Amount,
    max_sendable: Amount,
    description: String,
    nostr_pubkey: Option<String>,
    // If proxied cashu-lnurl created the invoice
    proxy: bool,
    cashu: Cashu,
    cln_client: Arc<Mutex<Option<ClnRpc>>>,
    db: Db,
    nostr: Nostr,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LnurlResponse {
    #[serde(with = "as_msat")]
    min_sendable: Amount,
    #[serde(with = "as_msat")]
    max_sendable: Amount,
    metadata: String,
    callback: Url,
    tag: LnurlTag,
    allows_nostr: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    nostr_pubkey: Option<String>,
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_lnurl_response_serialization() {
        let lnurl_response = LnurlResponse {
            min_sendable: Amount::from_sat(0),
            max_sendable: Amount::from_sat(1000),
            metadata: serde_json::to_string(&vec![vec![
                "text/plain".to_string(),
                "Hello world".to_string(),
            ]])
            .unwrap(),
            callback: Url::from_str("http://example.com").unwrap(),
            tag: LnurlTag::PayRequest,
            allows_nostr: true,
            nostr_pubkey: Some(
                "9630f464cca6a5147aa8a35f0bcdd3ce485324e732fd39e09233b1d848238f31".to_string(),
            ),
        };

        assert_eq!("{\"minSendable\":0,\"maxSendable\":1000000,\"metadata\":\"[[\\\"text/plain\\\",\\\"Hello world\\\"]]\",\"callback\":\"http://example.com/\",\"tag\":\"payRequest\",\"allowsNostr\":true,\"nostrPubkey\":\"9630f464cca6a5147aa8a35f0bcdd3ce485324e732fd39e09233b1d848238f31\"}", serde_json::to_string(&lnurl_response).unwrap());
    }
}
