mod app;
mod library;

use gpui_kit::*;

// The default component bundle only ships ~100 icons; embed the few extra
// Lucide icons this app uses.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [Pencil, Trash, FilePlus, FolderPlus, FolderMinus, LibraryBig]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

actions!(papr2, [Quit]);

fn main() {
    let root = library::Library::default_root();
    if std::env::var_os("PAPR2_HOME").is_none() {
        match library::migrate_legacy(&root) {
            Ok(Some(n)) => eprintln!("papr2: migrated {n} papers to {}", root.display()),
            Ok(None) => {}
            Err(err) => eprintln!("papr2: migrating the old library failed: {err:#}"),
        }
    }

    let library = match library::Library::open(&root) {
        Ok(lib) => lib,
        Err(err) => {
            eprintln!("papr2: failed to open library: {err:#}");
            std::process::exit(1);
        }
    };

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_kit::component::Theme::sync_system_appearance(None, cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(760.)), cx)),
                titlebar: Some(TitlebarOptions {
                    title: Some("papr2".into()),
                    ..Default::default()
                }),
                app_id: Some("papr2".into()),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::PaprApp::new(library, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
