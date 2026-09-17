use super::*;
use cosmic::iced::futures::StreamExt;
use zbus::object_server::SignalEmitter;

// KDE preserves well-known names in its registry, unlike system-tray's watcher.
#[derive(Default)]
struct Registry {
    items: Vec<String>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Registry {
    async fn register_status_notifier_host(&self, _service: &str) {}

    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.items.push(service.to_owned());
        Self::status_notifier_item_registered(&emitter, service).await?;
        Ok(())
    }

    async fn unregister_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.items.retain(|item| item != service);
        Self::status_notifier_item_unregistered(&emitter, service).await?;
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.clone()
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
}

struct TrayApp;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl TrayApp {
    #[zbus(property)]
    fn id(&self) -> &str {
        "tray-lifecycle-test"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
}

struct DelayedApp {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl DelayedApp {
    #[zbus(property)]
    async fn id(&self) -> &str {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        "delayed-tray-item"
    }
}

async fn next_matching(
    events: &mut cosmic::iced::futures::channel::mpsc::Receiver<Event>,
    predicate: impl Fn(&Event) -> bool,
) -> Event {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.next().await.expect("tray session ended");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("tray lifecycle event was not delivered")
}

fn private_bus_child(test: &str) -> bool {
    // Never host a watcher on the developer's desktop or mutate the
    // environment of parallel tests.
    const CHILD: &str = "COSMICBAR_TRAY_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return true;
    }
    let status = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("modules::tray::tests::{test}"),
            "--nocapture",
        ])
        .env(CHILD, test)
        .status()
        .expect("dbus-run-session is required for tray lifecycle tests");
    assert!(status.success(), "private-bus tray lifecycle test failed");
    false
}

#[tokio::test]
async fn watcher_removals_clear_items_and_selected_menu() {
    if !private_bus_child("watcher_removals_clear_items_and_selected_menu") {
        return;
    }

    let watcher = zbus::connection::Builder::session()
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at("/StatusNotifierWatcher", Registry::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let control = zbus::Proxy::new(
        &watcher,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await
    .unwrap();
    let (mut sender, mut events) = cosmic::iced::futures::channel::mpsc::channel(32);
    let task = tokio::spawn(async move { session(&mut sender).await.unwrap() });
    let connected = next_matching(&mut events, |event| matches!(event, Event::Connected(_))).await;
    let mut state = State::default();
    drop(state.update(connected));

    // Cover both KDE's well-known name and the unique-name/custom-path form
    // emitted by libappindicator and the built-in watcher. Keep a live neighbor.
    let survivor = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ITEM_OBJECT, TrayApp)
        .unwrap()
        .build()
        .await
        .unwrap();
    let survivor_address = survivor.unique_name().unwrap().as_str();
    let survivor_entry = format!("{survivor_address}{ITEM_OBJECT}");
    control
        .call::<_, _, ()>("RegisterStatusNotifierItem", &(&survivor_entry,))
        .await
        .unwrap();
    let added = next_matching(
        &mut events,
        |event| matches!(event, Event::Added(address, _) if &**address == survivor_address),
    )
    .await;
    drop(state.update(added));

    for (name, path) in [
        (Some("org.kde.StatusNotifierItem_123_1"), ITEM_OBJECT),
        (None, "/org/ayatana/NotificationItem/test"),
    ] {
        let mut builder = zbus::connection::Builder::session()
            .unwrap()
            .serve_at(path, TrayApp)
            .unwrap();
        if let Some(name) = name {
            builder = builder.name(name).unwrap();
        }
        let app = builder.build().await.unwrap();
        let address = name.unwrap_or(app.unique_name().unwrap().as_str());
        let entry = format!("{address}{path}");
        control
            .call::<_, _, ()>("RegisterStatusNotifierItem", &(&entry,))
            .await
            .unwrap();
        let added = next_matching(
            &mut events,
            |event| matches!(event, Event::Added(found, _) if &**found == address),
        )
        .await;
        drop(state.update(added));
        assert!(state.item(address).is_some());
        state.selected = Some(Arc::from(address));
        state.expanded.insert(7);

        // KDE reports a vanished well-known service, while an appindicator
        // can unregister its custom object on a still-live bus connection.
        if name.is_some() {
            app.clone().close().await.unwrap();
        }
        control
            .call::<_, _, ()>("UnregisterStatusNotifierItem", &(&entry,))
            .await
            .unwrap();
        let removed = next_matching(
            &mut events,
            |event| matches!(event, Event::Removed(found) if &**found == address),
        )
        .await;
        drop(state.update(removed));
        assert!(state.item(address).is_none());
        assert!(state.selected.is_none());
        assert!(state.expanded.is_empty());
        assert!(state.item(survivor_address).is_some());
    }

    // Hold GetAll until after unregister has been consumed. A late client Add
    // must not resurrect the removed item. The subsequent registration is a
    // barrier: system-tray discovers these registrations in stream order.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let delayed = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(
            ITEM_OBJECT,
            DelayedApp {
                entered: entered.clone(),
                release: release.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let address = delayed.unique_name().unwrap().as_str();
    let entry = format!("{address}{ITEM_OBJECT}");
    control
        .call::<_, _, ()>("RegisterStatusNotifierItem", &(&entry,))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    control
        .call::<_, _, ()>("UnregisterStatusNotifierItem", &(&entry,))
        .await
        .unwrap();
    next_matching(
        &mut events,
        |event| matches!(event, Event::Removed(found) if &**found == address),
    )
    .await;
    release.add_permits(1);
    let barrier = zbus::connection::Builder::session()
        .unwrap()
        .serve_at(ITEM_OBJECT, TrayApp)
        .unwrap()
        .build()
        .await
        .unwrap();
    let barrier_address = barrier.unique_name().unwrap().as_str();
    let barrier_entry = format!("{barrier_address}{ITEM_OBJECT}");
    control
        .call::<_, _, ()>("RegisterStatusNotifierItem", &(&barrier_entry,))
        .await
        .unwrap();
    next_matching(&mut events, |event| {
        assert!(
            !matches!(event, Event::Added(found, _) if &**found == address),
            "late discovery resurrected an unregistered tray item"
        );
        matches!(event, Event::Added(found, _) if &**found == barrier_address)
    })
    .await;
    task.abort();
}

#[tokio::test]
async fn builtin_watcher_removes_disconnected_items() {
    if !private_bus_child("builtin_watcher_removes_disconnected_items") {
        return;
    }
    let (mut sender, mut events) = cosmic::iced::futures::channel::mpsc::channel(32);
    let task = tokio::spawn(async move { session(&mut sender).await.unwrap() });
    next_matching(&mut events, |event| matches!(event, Event::Connected(_))).await;

    let app = zbus::connection::Builder::session()
        .unwrap()
        .serve_at("/org/ayatana/NotificationItem/test", TrayApp)
        .unwrap()
        .build()
        .await
        .unwrap();
    let address = app.unique_name().unwrap().to_string();
    let watcher = zbus::Proxy::new(
        &app,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await
    .unwrap();
    watcher
        .call::<_, _, ()>(
            "RegisterStatusNotifierItem",
            &("/org/ayatana/NotificationItem/test",),
        )
        .await
        .unwrap();
    next_matching(
        &mut events,
        |event| matches!(event, Event::Added(found, _) if &**found == address),
    )
    .await;
    drop(watcher);
    app.close().await.unwrap();
    next_matching(
        &mut events,
        |event| matches!(event, Event::Removed(found) if &**found == address),
    )
    .await;
    task.abort();
}
