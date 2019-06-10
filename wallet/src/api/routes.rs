use actix::prelude::*;
use futures::{future, Future};
use jsonrpc_core::{Middleware, Params};
use jsonrpc_pubsub::{PubSubHandler, PubSubMetadata};
use serde_json::{json, to_value};

use crate::actors::app::App;
use crate::api;
use crate::error;

/// Helper macro to add multiple JSON-RPC methods at once
macro_rules! routes {
    ($io:expr, $app:expr $(,)?) => {};
    ($io:expr, $app:expr, ($method_jsonrpc:expr, $actor_msg:ty $(,)?), $($args:tt)*) => {
        {
            let app_addr = $app.clone();
            $io.add_method($method_jsonrpc, move |params: Params| {
                log::debug!("Handling request for method: {}", $method_jsonrpc);
                let addr = app_addr.clone();
                // Try to parse the request params into the actor message
                future::result(params.parse::<$actor_msg>())
                    .and_then(move |msg| {
                        // Then send the parsed message to the actor
                        addr.send(msg)
                            .map_err(error::Error::Mailbox)
                            .flatten()
                            .and_then(
                                |x|
                                future::result(to_value(x)).map_err(error::Error::Serialization)
                            )
                            .map_err(|err| error::ApiError::Execution(err).into())
                    })
            });
        }
        routes!($io, $app, $($args)*);
    };
}

/// Macro to add multiple JSON-RPC methods that forward the request to the Node at once
macro_rules! forwarded_routes {
    ($io:expr, $app:expr $(,)?) => {};
    ($io:expr, $app:expr, $method:expr, $($args:tt)*) => {
        {
            let app_addr = $app.clone();
            $io.add_method($method, move |params: Params| {
                log::debug!("Forwarding request for method: {}", $method);
                let msg = api::ForwardRequest {
                    method: $method.to_string(),
                    params
                };
                app_addr.send(msg)
                    .map_err(error::Error::Mailbox)
                    .flatten()
                    .and_then(|x| {
                        future::result(to_value(x)).map_err(error::Error::Serialization)
                    })
                    .map_err(|err| error::ApiError::Execution(err).into())
            });
        }
        forwarded_routes!($io, $app, $($args)*);
    };
}

pub fn connect_routes<T, S>(handler: &mut PubSubHandler<T, S>, app: Addr<App>)
where
    T: PubSubMetadata,
    S: Middleware<T>,
{
    handler.add_subscription(
        "notifications",
        ("subscribeNotifications", {
            let addr = app.clone();
            move |_, _, subscriber| addr.do_send(api::SubscribeRequest(subscriber))
        }),
        ("unsubscribeNotifications", {
            let addr = app.clone();
            move |id, _| {
                addr.send(api::UnsubscribeRequest(id))
                    .map_err(error::Error::Mailbox)
                    .and_then(|_| future::ok(json!({"status": "ok"})))
                    .map_err(|err| error::ApiError::Execution(err).into())
            }
        }),
    );

    forwarded_routes!(
        handler,
        app,
        "getBlock",
        "getBlockChain",
        "getOutput",
        "inventory",
    );

    routes!(
        handler,
        app,
        ("getWalletInfos", api::WalletInfosRequest),
        ("createMnemonics", api::CreateMnemonicsRequest),
        ("importSeed", api::ImportSeedRequest),
        ("createWallet", api::CreateWalletRequest),
        ("lockWallet", api::LockWalletRequest),
        ("unlockWallet", api::UnlockWalletRequest),
        ("getTransactions", api::GetTransactionsRequest),
        ("sendVTT", api::SendVttRequest),
        ("generateAddress", api::GenerateAddressRequest),
        ("createDataRequest", api::CreateDataReqRequest),
        ("runRadRequest", api::RunRadReqRequest),
        ("sendDataRequest", api::SendDataReqRequest),
    );
}