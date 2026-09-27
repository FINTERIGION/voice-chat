mod app;
mod audio;
mod avatar;
mod dashscope;
mod i18n;
mod llm;
mod memory;
mod prompt;
mod realtime;
mod secrets;
mod store;
mod subtitle;
mod tray;
mod voice;

use std::sync::Mutex;

use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use app::state::AppState;

pub(crate) const DEFAULT_HOTKEY: &str = "Ctrl+Shift+Space";

fn hotkey_handler(app: &AppHandle, _shortcut: &Shortcut, event: ShortcutEvent) {
    if event.state() != ShortcutState::Pressed {
        return;
    }
    tracing::info!("hotkey pressed: toggling mic");
    app.state::<AppState>().session.toggle_talking();
}

/// Registers `accelerator` as the mic-toggle hotkey. Shared by initial
/// startup (reading the stored setting, or the default if never set) and
/// `app::commands::set_hotkey` (changing it at runtime) so there's exactly
/// one place that knows how the hotkey is wired up.
pub(crate) fn install_hotkey(app: &AppHandle, accelerator: &str) -> Result<(), String> {
    app.global_shortcut()
        .on_shortcut(accelerator, hotkey_handler)
        .map_err(|e| e.to_string())
}

/// First launch has no characters yet; seed one so the app is immediately
/// usable, and make sure `current_character_id` always points at something.
fn seed_default_character(conn: &rusqlite::Connection) -> Result<(), Box<dyn std::error::Error>> {
    if store::character::count(conn)? == 0 {
        let default = store::character::create(
            conn,
            store::character::CharacterInput::default_new(
                "小柔",
                "温柔体贴、爱聊天的陪伴助手，喜欢倾听并给出简短实用的建议。",
                "语气自然亲切，偶尔用口语化的语气词，句子简短。",
            ),
        )?;
        store::db::set_setting(conn, "current_character_id", &default.id)?;
    } else if store::db::get_setting(conn, "current_character_id")?
        .filter(|s| !s.is_empty())
        .is_none()
    {
        if let Some(first) = store::character::list(conn)?.into_iter().next() {
            store::db::set_setting(conn, "current_character_id", &first.id)?;
        }
    }
    Ok(())
}

/// Makes sure the built-in "Emma" English-speaking-coach character exists,
/// without touching `current_character_id` — unlike `seed_default_character`
/// this doesn't only run on a brand-new database, since existing users
/// should get the new preset too. Matched by name so re-running on every
/// launch doesn't create duplicates.
fn seed_english_coach_character(
    conn: &rusqlite::Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    let already_exists = store::character::list(conn)?
        .iter()
        .any(|c| c.name == "Emma");
    if already_exists {
        return Ok(());
    }
    store::character::create(
        conn,
        store::character::CharacterInput {
            name: "Emma".into(),
            avatar_path: None,
            language: "en".into(),
            persona: "Emma 是一位耐心、鼓励式的英语口语教练兼对话伙伴，帮助用户在真实场景中练习英语口语。\
                她会主动开话题、提供角色扮演场景（比如点餐、面试、旅行问路、朋友闲聊），引导用户多开口、多表达；\
                当用户卡壳、说错或语法不通顺时，她不会打断或列语法规则，而是在下一句里自然地把正确说法复述一遍（recast），\
                需要时再简短给出更地道的替代说法；她会记住用户反复出现的错误和生词，后续对话里针对性地再练习；\
                她始终保持积极肯定，会具体地表扬用户说得好的地方，不让用户觉得被评判或尴尬。".into(),
            speech_habits: "以简单清晰、语速适中的英语为主，句子短、口语化，像真实聊天而不是上课；\
                多用提问和延伸话题鼓励用户开口，而不是自己长篇讲解；纠错只靠自然重述正确说法，不讲语法术语；\
                如果用户明显听不懂、卡住很久，或直接用中文提问，可以用一两句简短中文解释或给提示，然后立刻用英语继续对话。".into(),
            voice_kind: "preset".into(),
            voice_id: Some("longanqian".into()),
            voice_prompt: None,
            memory_enabled: true,
            max_history_turns: 20,
        },
    )?;
    Ok(())
}

/// `reqwest` turns on rustls's aws-lc-rs provider and `tauri-plugin-updater`
/// turns on ring. Cargo unifies those features, so both are compiled in, and rustls
/// then refuses to guess which one a handshake should use — it panics instead.
/// That panic lands in the realtime socket, after the chat UI has already been
/// told the session is connecting, so the screen stays on "连接中".
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // Must be the first plugin. A second launch exits while plugins are
        // initialized in `build` — before `setup` below, whose sweeps and
        // closing of dangling conversations would otherwise hit the data of
        // the instance that's still running — after asking that instance to
        // bring its window back, which matters since it may be in the tray.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main_window(app);
        }))
        // Character pictures, as `<img src>` for the webview — see `avatar`.
        .register_uri_scheme_protocol("avatar", |ctx, request| {
            let state = ctx.app_handle().try_state::<AppState>();
            avatar::serve(
                state.as_ref().map(|s| s.avatars_dir.as_path()),
                request.uri().path(),
            )
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let app_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_dir)?;
            let conn = store::db::open(&app_dir.join("voice-chat.db"))?;
            seed_default_character(&conn)?;
            seed_english_coach_character(&conn)?;
            // The session actor is spawned further down, so nothing is live
            // yet and every conversation still open is one the last run never
            // got to close.
            match store::message::close_dangling_conversations(&conn) {
                Ok(0) => {}
                Ok(n) => tracing::info!("closed {n} conversation(s) left open by a previous run"),
                Err(e) => tracing::error!("failed to close dangling conversations: {e}"),
            }
            // No window exists yet, so no editor can be holding a picture it
            // hasn't saved: whatever no character points at is an orphan.
            let avatars_dir = app_dir.join(avatar::DIR_NAME);
            std::fs::create_dir_all(&avatars_dir)?;
            let in_use = store::character::list(&conn)?
                .into_iter()
                .filter_map(|c| c.avatar_path)
                .collect();
            match avatar::sweep(&avatars_dir, &in_use) {
                0 => {}
                n => tracing::info!("removed {n} avatar(s) no character uses"),
            }
            // Samples are recorded against their voice as soon as they are
            // written, but a backup restore writes its files before the
            // transaction that points at them, and a restore that then fails
            // leaves them behind.
            let voice_samples_dir = app_dir.join(voice::sample::DIR_NAME);
            std::fs::create_dir_all(&voice_samples_dir)?;
            let kept = store::voice_sample::file_names(&conn)?;
            match voice::sample::sweep(&voice_samples_dir, &kept) {
                0 => {}
                n => tracing::info!("removed {n} voice sample(s) no voice uses"),
            }
            // `None` (key never set) means "use the default"; `Some("")`
            // means the user explicitly disabled the hotkey in Settings.
            let hotkey_setting = store::db::get_setting(&conn, "hotkey")?;
            // Read here rather than after `conn` is moved into `AppState`
            // below — `subtitle::open` re-locks it fresh once that's done.
            let subtitle_settings = subtitle::get_settings(&conn)?;

            // Applied before anything can fail below, so even a startup
            // error surfaces in the language the user picked.
            i18n::set(match store::db::get_setting(&conn, "ui_language")? {
                Some(tag) if !tag.is_empty() => i18n::Lang::from_tag(&tag),
                _ => i18n::DEFAULT,
            });

            tray::create(app.handle())?;

            let session = realtime::session::spawn(app.handle().clone());
            app.manage(AppState {
                db: Mutex::new(conn),
                session,
                recorder: Mutex::new(None),
                avatars_dir,
                voice_samples_dir,
                pending_update: Mutex::new(None),
            });
            // After the sweep above, so what it closed can be written.
            realtime::session::spawn_memory_backfill(app.handle().clone());

            let hotkey = match hotkey_setting {
                None => Some(DEFAULT_HOTKEY.to_string()),
                Some(s) if s.is_empty() => None,
                Some(s) => Some(s),
            };
            if let Some(accel) = hotkey {
                if let Err(e) = install_hotkey(app.handle(), &accel) {
                    tracing::error!("failed to register hotkey {accel:?}: {e}");
                }
            }

            if subtitle_settings.enabled {
                if let Err(e) = subtitle::open(app.handle()) {
                    tracing::error!("failed to open subtitle window: {e}");
                }
            }

            Ok(())
        })
        // Closing the main window sends the app to the tray instead of
        // quitting it; `tray` is where it's brought back or really exited.
        .on_window_event(|window, event| {
            if window.label() != tray::MAIN_WINDOW {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(e) = window.hide() {
                    tracing::error!("failed to hide main window: {e}");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app::commands::get_secret_status,
            app::commands::set_api_key,
            app::commands::clear_api_key,
            app::commands::get_connection_settings,
            app::commands::set_connection_settings,
            app::commands::list_regions,
            app::commands::get_ui_language,
            app::commands::set_ui_language,
            app::commands::get_vad_settings,
            app::commands::set_vad_settings,
            app::commands::test_connectivity,
            app::commands::start_talking,
            app::commands::stop_talking,
            app::commands::toggle_talking,
            app::commands::get_mic_open,
            app::commands::get_hotkey,
            app::commands::set_hotkey,
            app::commands::get_subtitle_settings,
            app::commands::set_subtitle_settings,
            app::commands::set_subtitle_adjusting,
            app::commands::interrupt,
            app::commands::set_recording,
            app::commands::list_memories,
            app::commands::update_memory,
            app::commands::delete_memory,
            app::commands::delete_memories,
            app::commands::list_conversations,
            app::commands::get_conversation_messages,
            app::commands::get_active_conversation_id,
            app::commands::new_conversation,
            app::commands::rename_conversation,
            app::commands::delete_conversation,
            app::commands::list_characters,
            app::commands::create_character,
            app::commands::update_character,
            app::commands::delete_character,
            app::commands::get_current_character_id,
            app::commands::switch_character,
            app::commands::set_character_avatar,
            app::commands::save_avatar,
            app::commands::generate_avatar,
            app::commands::polish_persona,
            app::commands::list_preset_voices,
            app::commands::start_recording,
            app::commands::stop_recording,
            app::commands::clone_voice,
            app::commands::get_voice_sample,
            app::commands::design_voice_preview,
            app::commands::discard_design_previews,
            app::commands::slugify,
            app::commands::list_voices,
            app::commands::delete_voice,
            app::commands::export_backup,
            app::commands::import_backup,
            app::commands::export_character,
            app::commands::open_character_file,
            app::commands::import_character,
            app::commands::check_for_update,
            app::commands::install_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                app_handle.state::<AppState>().session.shutdown();
            }
        });
}
