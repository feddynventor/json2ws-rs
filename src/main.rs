use clap::Parser;
use env_logger;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, RwLock};
use warp::{
    ws::{Message, WebSocket},
    Filter,
};

type Clients = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<Message>>>>;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short = 'p', long, default_value_t = 3000)]
    http_port: u16,

    #[arg(short = 'w', long, default_value_t = 8080)]
    ws_port: u16,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let args = Args::parse();

    let clients: Clients = Arc::new(RwLock::new(HashMap::new()));
    let clients_ws = clients.clone();

    let ws_route = warp::path::end()
        .and(warp::ws())
        .and(warp::any().map(move || clients_ws.clone()))
        .map(|ws: warp::ws::Ws, clients| {
            ws.on_upgrade(move |socket| handle_websocket_connection(socket, clients))
        });

    // JSON broadcast route (HTTP POST endpoint)
    let broadcast_post_route = warp::path("broadcast")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_clients(clients.clone()))
        .and_then(handle_broadcast);

    // Query param broadcast route (HTTP GET endpoint)
    let broadcast_get_route = warp::path("broadcast")
        .and(warp::get())
        .and(warp::query::<HashMap<String, String>>())
        .and(with_clients(clients.clone()))
        .and_then(|params: HashMap<String, String>, clients: Clients| {
            // Convert query parameters to JSON Value
            let payload = json!(params);
            handle_broadcast(payload, clients)
        });

    // Health check route
    let health_route = warp::path("health").map(|| "OK");

    let ws_server = warp::serve(ws_route)
        .run(([0, 0, 0, 0], args.ws_port));

    let http_server = warp::serve(broadcast_post_route.or(health_route).or(broadcast_get_route))
        .run(([0, 0, 0, 0], args.http_port));

    info!("WebSocket server started on port {}", args.ws_port);
    info!(
        "HTTP server for broadcasting started on port {}",
        args.http_port
    );

    // Run both servers concurrently
    tokio::join!(ws_server, http_server);
}

// Helper function to share clients with routes
fn with_clients(clients: Clients) -> impl Filter<Extract = (Clients,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || clients.clone())
}

async fn handle_websocket_connection(ws: WebSocket, clients: Clients) {
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Use a channel to handle messages going to the client
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client_id = uuid::Uuid::new_v4().to_string();

    clients.write().await.insert(client_id.clone(), tx);
    info!("Client connected: {}", client_id);

    // Clone the client_id before moving it into the task
    let task_client_id = client_id.clone();

    // Task that forwards messages from the channel to the WebSocket
    tokio::task::spawn(async move {
        while let Some(message) = rx.recv().await {
            match ws_tx.send(message).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Error sending message to client {}: {}", task_client_id, e);
                    break;
                }
            }
        }

        // close the WebSocket gracefully
        let _ = ws_tx.close().await;
    });

    // Handle incoming WebSocket connection, pings, connection close messages
    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) => {
                // Handle ping/pong or other control messages if needed
                if msg.is_ping() {
                    let _ = clients
                        .read()
                        .await
                        .get(&client_id)
                        .unwrap()
                        .send(Message::pong(Vec::new()));
                }
            }
            Err(e) => {
                error!("Error receiving message from client {}: {}", client_id, e);
                break;
            }
        }
    }

    // Client disconnected or error occurred, remove from the list
    clients.write().await.remove(&client_id);
    info!("Client disconnected: {}", client_id);
}

async fn handle_broadcast(
    payload: Value,
    clients: Clients,
) -> Result<impl warp::Reply, warp::Rejection> {
    let json_str = serde_json::to_string(&payload).expect("Failed to serialize JSON payload");

    // let client_count = clients.read().await.len();
    // info!("Broadcasting to {} clients", client_count);

    // if client_count > 0 {
    let message = Message::text(json_str);

    let mut disconnected_clients = Vec::new();

    for (client_id, tx) in clients.read().await.iter() {
        if tx.send(message.clone()).is_err() {
            // Client might have disconnected, mark for removal
            disconnected_clients.push(client_id.clone());
        }
    }

    // Clean up any clients that failed to receive the message
    if !disconnected_clients.is_empty() {
        let mut clients = clients.write().await;
        for client_id in disconnected_clients {
            clients.remove(&client_id);
            warn!("Removed stale client: {}", client_id);
        }
    }
    // }

    // Use json! macro for proper type handling
    Ok(warp::reply::json(&json!({
        "success": true,
        // "clients": client_count
    })))
}
