#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod methods;
use gveditor_core::gen_client::Client;
use gveditor_core::handlers::{LocalHandler, TransportHandler};
use gveditor_core::tokio::sync::mpsc::{channel, Receiver, Sender};
use gveditor_core::{tokio, Configuration, Server};
use gveditor_core_api::extensions::manager::ExtensionsManager;
use gveditor_core_api::messaging::{ClientMessages, ServerMessages};
use gveditor_core_api::state_persistors::file::FilePersistor;
use gveditor_core_api::states::{StatesList, TokenFlags};
use gveditor_core_api::{Mutex, State};
use gveditor_core_deno::DenoExtensionSupport;
use std::fs;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::api::path::{resolve_path, BaseDirectory};
use tauri::utils::assets::EmbeddedAssets;
use tauri::{Context, Env, Manager};
use tracing::{error, info, warn};
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

#[cfg(any(target_os = "windows"))]
use window_shadows::set_shadow;

/// The app backend state
pub struct TauriState {
    client: Client,
}

fn open_window(
    context: Context<EmbeddedAssets>,
    client: Client,
    sender_to_handler: Sender<ClientMessages>,
    mut receiver_from_handler: Receiver<ServerMessages>,
) -> tauri::Result<()> {
    tauri::Builder::default()
        .setup(move |app| {

            let window = app.get_window("main").unwrap();

            #[cfg(any(target_os = "windows"))]
            set_shadow(&window, true).unwrap();

            // Forward messages from the webview to the core
            window.listen("to_core", move |event| {
                let sender_to_handler = sender_to_handler.clone();
                let event_payload = event.payload();

                if let Some(event_payload) = event_payload {
                    let event: Result<ClientMessages, serde_json::Error> = serde_json::from_str(event_payload);

                    if let Ok(event) = event {
                        tokio::task::spawn(async move {
                            info!("Event Webview -> Core, event: {event:?}");
                            sender_to_handler.send(event).await.unwrap();
                        });
                    } else {
                        error!("Received a message from webview with non-JSON payload, content: {event_payload}");
                    }
                } else {
                    warn!("Received a message from webview without payload");
                }
            });

            // Forward messages from the core to the webview
            tokio::spawn(async move {
                loop {
                    if let Some(event) = receiver_from_handler.recv().await {
                        info!("Event Core -> Webview, event: {event:?}");
                        window.emit("to_webview", event).unwrap();
                    }
                }
            });

            Ok(())
        })
        .manage(TauriState { client })
        .invoke_handler(tauri::generate_handler![
            methods::get_state_by_id,
            methods::list_dir_by_path,
            methods::write_file_by_path,
            methods::read_file_by_path,
            methods::set_state_by_id,
            methods::get_ext_info_by_id,
            methods::get_ext_list,
            methods::get_all_language_server_builders,
            methods::write_to_terminal_shell,
            methods::create_terminal_shell,
            methods::close_terminal_shell,
            methods::get_terminal_shell_builders,
            methods::resize_terminal_shell,
            methods::create_language_server,
            methods::write_to_language_server
        ])
        .run(context)
}

/// Returns the location in which where to save the settings and state
///
/// # Arguments
///
/// * `context` - The Tauri Context
///
fn get_settings_path(context: &Context<EmbeddedAssets>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let settings_path = resolve_path(
        context.config(),
        context.package_info(),
        &Env::default(),
        ".graviton/states",
        Some(BaseDirectory::Home),
    )?;

    fs::create_dir_all(&settings_path)?;

    let settings_file_path = settings_path.join("settings.json");

    File::create(&settings_file_path)?;

    Ok((settings_path, settings_file_path))
}

/// Returns the path where third-party extensions are installed and loaded from
///
/// # Arguments
///
/// * `context` - The Tauri Context
///
fn get_extensions_installation_path(context: &Context<EmbeddedAssets>) -> anyhow::Result<PathBuf> {
    let extensions_installation_path = resolve_path(
        context.config(),
        context.package_info(),
        &Env::default(),
        ".graviton/extensions",
        Some(BaseDirectory::Home),
    )?;

    fs::create_dir_all(&extensions_installation_path)?;

    Ok(extensions_installation_path)
}

/// Setup the logger
fn setup_logger() {
    let filter = EnvFilter::default()
        .add_directive("graviton=info".parse().unwrap())
        .add_directive("gveditor_core_api=info".parse().unwrap())
        .add_directive("gveditor_core=info".parse().unwrap())
        .add_directive("typescript_lsp_graviton=info".parse().unwrap());

    let subscriber = Registry::default().with(filter).with(fmt::Layer::default());

    tracing::subscriber::set_global_default(subscriber).expect("Unable to set global subscriber");
}

// Dummy token
static TOKEN: &str = "graviton_token";
static STATE_ID: u8 = 1;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logger();

    let (to_core, from_core) = channel::<ClientMessages>(10000);

    let context = tauri::generate_context!("tauri.conf.json");

    // Get the Settings paths
    let settings_paths = get_settings_path(&context);

    if let Err(err) = &settings_paths {
        error!("Could not get the settings path: {err}");
    }

    let (settings_path, settings_file_path) = settings_paths?;

    let mut extensions_manager =
        ExtensionsManager::new(to_core.clone(), Some(settings_path.clone()));

    let third_party_extensions_path = get_extensions_installation_path(&context);

    if let Err(err) = &third_party_extensions_path {
        error!("Could not get the settings path, error: {err}");
    }

    let third_party_extensions_path = third_party_extensions_path?;

    // Load built-in extensions
    extensions_manager
        .load_extension_from_entry(
            git_for_graviton::entry,
            git_for_graviton::get_info(),
            STATE_ID,
        )
        .await
        .load_extension_from_entry(
            typescript_lsp_graviton::entry,
            typescript_lsp_graviton::get_info(),
            STATE_ID,
        )
        .await
        .load_extension_from_entry(
            native_shell_graviton::entry,
            native_shell_graviton::get_info(),
            STATE_ID,
        )
        .await;

    // Load third party extensions
    extensions_manager
        .load_extensions_with_deno_in_directory(
            third_party_extensions_path.to_str().unwrap(),
            STATE_ID,
        )
        .await;

    // Create the StatesList
    let states = {
        let default_state = State::new(
            STATE_ID,
            extensions_manager,
            Box::new(FilePersistor::new(settings_file_path)),
        );
        let states = StatesList::new()
            .with_tokens(&[TokenFlags::All(TOKEN.to_string())])
            .with_state(default_state);

        Arc::new(Mutex::new(states))
    };

    // Sender and receiver for the webview window
    let (to_webview, from_handler) = channel(100);

    // Create the Local handler transport
    let (local_handler, client, to_local) = LocalHandler::new(states.clone(), to_webview);
    let local_handler: Box<dyn TransportHandler + Send + Sync> = Box::new(local_handler);

    let config = Configuration::new(local_handler, to_core, from_core);

    let core = Server::new(config, states);

    core.run().await;

    // Open the window
    let res = open_window(context, client, to_local, from_handler);

    if let Err(err) = res {
        error!("Graviton crashed, error: {err}");
        Err(anyhow::Error::from(err))
    } else {
        Ok(())
    }
}
