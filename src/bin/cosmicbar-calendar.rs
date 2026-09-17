//! Popup-scoped Evolution Data Server worker. No interpreter or GI startup.
//! Process isolation lets the bar stop even an unresponsive native backend.
#![allow(unsafe_op_in_unsafe_fn)]

use jiff::{Zoned, civil::Date, tz::TimeZone};
use serde::Serialize;
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{CStr, CString, c_void},
    io::{self, Write},
    ptr,
    sync::mpsc,
    time::Duration,
};

#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/calendar.rs"));
}
use ffi::*;

#[derive(Serialize, PartialEq, Eq)]
struct Event {
    title: String,
    time: String,
}

fn fail() -> ! {
    let _ = writeln!(
        io::stdout().lock(),
        "{}",
        serde_json::json!({"date": Zoned::now().date().to_string(), "error": "Calendar connection failed; check GNOME Calendar and Evolution Data Server"})
    );
    std::process::exit(1)
}

// Every potentially blocking native call is bounded; no idle watchdog polling.
struct Deadline(mpsc::Sender<bool>);
impl Deadline {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(active) = rx.recv() {
                if active && rx.recv_timeout(Duration::from_secs(20)).is_err() {
                    fail();
                }
            }
        });
        Self(tx)
    }
    fn call<T>(&self, call: impl FnOnce() -> T) -> T {
        self.0.send(true).unwrap();
        let result = call();
        self.0.send(false).unwrap();
        result
    }
}

struct Object<T>(*mut T);
impl<T> Drop for Object<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                g_object_unref(self.0.cast());
            }
        }
    }
}

#[derive(Default)]
struct Changes {
    sources: Cell<bool>,
    events: Cell<bool>,
    failed: Cell<bool>,
}
unsafe extern "C" fn source_changed(_: *mut c_void, _: *mut c_void, data: *mut c_void) {
    (*(data as *const Changes)).sources.set(true);
}
unsafe extern "C" fn events_changed(_: *mut c_void, _: *mut c_void, data: *mut c_void) {
    (*(data as *const Changes)).events.set(true);
}
unsafe extern "C" fn completed(_: *mut c_void, error: *mut c_void, data: *mut c_void) {
    let changes = &*(data as *const Changes);
    if error.is_null() {
        // Start acknowledges before backend enumeration finishes. With initial
        // adds suppressed, reconcile once at the completion fence so changes
        // between our fast first query and subscription readiness cannot vanish.
        changes.events.set(true);
    } else {
        changes.failed.set(true);
    }
}
unsafe extern "C" fn backend_died(_: *mut c_void, data: *mut c_void) {
    (*(data as *const Changes)).failed.set(true);
}
unsafe extern "C" fn clock_tick(_: *mut c_void) -> i32 {
    let now = Zoned::now();
    let midnight = now
        .date()
        .tomorrow()
        .unwrap()
        .to_zoned(now.time_zone().clone())
        .unwrap();
    let delay = (midnight.timestamp().as_millisecond() - now.timestamp().as_millisecond() + 1)
        .clamp(1, 60_000) as u32;
    g_timeout_add(delay, Some(clock_tick), ptr::null_mut());
    0
}
unsafe extern "C" fn source_service_lost(
    _: *mut GDBusConnection,
    _: *const std::ffi::c_char,
    _: *mut c_void,
) {
    fail();
}
unsafe extern "C" fn backend_error(_: *mut c_void, _: *mut c_void, data: *mut c_void) {
    (*(data as *const Changes)).failed.set(true);
}
unsafe fn signal<T>(
    object: *mut T,
    name: &CStr,
    callback: unsafe extern "C" fn(),
    changes: &Changes,
) {
    g_signal_connect_data(
        object.cast(),
        name.as_ptr(),
        Some(callback),
        (changes as *const Changes).cast_mut().cast(),
        None,
        0,
    );
}
unsafe fn check(error: *mut GError) {
    if !error.is_null() {
        g_error_free(error);
        fail();
    }
}

struct Calendar {
    view: Object<ECalClientView>,
    client: Object<ECalClient>,
}
unsafe fn reconcile(
    registry: *mut ESourceRegistry,
    calendars: &mut BTreeMap<String, Calendar>,
    changes: &Changes,
    deadline: &Deadline,
) -> bool {
    let list = e_source_registry_list_sources(registry, c"Calendar".as_ptr());
    let mut cursor = list;
    let mut selected = BTreeMap::new();
    while !cursor.is_null() {
        let source = Object((*cursor).data.cast::<ESource>());
        if e_source_registry_check_enabled(registry, source.0) != 0
            && e_source_selectable_get_selected(
                e_source_get_extension(source.0, c"Calendar".as_ptr()).cast(),
            ) != 0
        {
            selected.insert(
                CStr::from_ptr(e_source_get_uid(source.0))
                    .to_string_lossy()
                    .into_owned(),
                source,
            );
        }
        cursor = (*cursor).next;
    }
    g_list_free(list);
    let changed = calendars.keys().ne(selected.keys());
    calendars.retain(|uid, calendar| {
        if selected.contains_key(uid) {
            true
        } else {
            deadline.call(|| {
                let mut error = ptr::null_mut();
                e_cal_client_view_stop(calendar.view.0, &mut error);
                check(error);
            });
            false
        }
    });
    for (uid, source) in selected {
        if calendars.contains_key(&uid) {
            continue;
        }
        let calendar = deadline.call(|| {
            let mut error = ptr::null_mut();
            // EDS documents UINT_MAX (-1) as skipping remote-connect waits.
            let client = Object(
                e_cal_client_connect_sync(
                    source.0,
                    ECalClientSourceType_E_CAL_CLIENT_SOURCE_TYPE_EVENTS,
                    u32::MAX,
                    ptr::null_mut(),
                    &mut error,
                )
                .cast::<ECalClient>(),
            );
            check(error);
            if client.0.is_null() {
                fail();
            }
            let mut view = ptr::null_mut();
            if e_cal_client_get_view_sync(
                client.0,
                c"#t".as_ptr(),
                &mut view,
                ptr::null_mut(),
                &mut error,
            ) == 0
            {
                check(error);
                fail();
            }
            let view = Object(view);
            e_cal_client_view_set_flags(view.0, 0, &mut error);
            check(error);
            let fields = GSList {
                data: c"UID".as_ptr().cast_mut().cast(),
                next: ptr::null_mut(),
            };
            e_cal_client_view_set_fields_of_interest(view.0, &fields, &mut error);
            check(error);
            for name in [c"objects-added", c"objects-modified", c"objects-removed"] {
                signal(
                    view.0,
                    name,
                    std::mem::transmute::<
                        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                        unsafe extern "C" fn(),
                    >(events_changed),
                    changes,
                );
            }
            signal(
                view.0,
                c"complete",
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                    unsafe extern "C" fn(),
                >(completed),
                changes,
            );
            signal(
                client.0,
                c"backend-died",
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void),
                    unsafe extern "C" fn(),
                >(backend_died),
                changes,
            );
            signal(
                client.0,
                c"backend-error",
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                    unsafe extern "C" fn(),
                >(backend_error),
                changes,
            );
            e_cal_client_view_start(view.0, &mut error);
            check(error);
            Calendar { view, client }
        });
        calendars.insert(uid, calendar);
    }
    changed
}

struct Collection {
    date: Date,
    start: i64,
    end: i64,
    zone: TimeZone,
    native_zone: *mut ICalTimezone,
    events: Vec<(i64, i64, Event)>,
}
impl Collection {
    unsafe fn timestamp(&self, value: *mut ICalTime, all_day: bool) -> i64 {
        if all_day {
            Date::new(
                i_cal_time_get_year(value) as i16,
                i_cal_time_get_month(value) as i8,
                i_cal_time_get_day(value) as i8,
            )
            .unwrap()
            .to_zoned(self.zone.clone())
            .unwrap()
            .timestamp()
            .as_second()
        } else {
            // Borrowed cached timezone; releasing it invalidates other times.
            let zone = i_cal_time_get_timezone(value);
            i_cal_time_as_timet_with_zone(
                value,
                if zone.is_null() {
                    self.native_zone
                } else {
                    zone
                },
            )
        }
    }
    fn clock(&self, stamp: i64) -> String {
        let value = jiff::Timestamp::from_second(stamp)
            .unwrap()
            .to_zoned(self.zone.clone());
        value
            .strftime(if value.date() == self.date {
                "%H:%M"
            } else {
                "%b %d %H:%M"
            })
            .to_string()
    }
}
unsafe extern "C" fn collect(
    component: *mut ICalComponent,
    first: *mut ICalTime,
    last: *mut ICalTime,
    data: *mut c_void,
    _: *mut GCancellable,
    _: *mut *mut GError,
) -> i32 {
    let output = &mut *data.cast::<Collection>();
    if i_cal_component_get_status(component) == ICalPropertyStatus_I_CAL_STATUS_CANCELLED {
        return 1;
    }
    let dtstart = Object(i_cal_component_get_dtstart(component));
    if dtstart.0.is_null() {
        return 1;
    }
    let all_day = i_cal_time_is_date(dtstart.0) != 0;
    let (first, last) = (
        output.timestamp(first, all_day),
        output.timestamp(last, all_day),
    );
    if first >= output.end || (last <= output.start && !(first == last && first >= output.start)) {
        return 1;
    }
    let time = if all_day {
        "All day".to_owned()
    } else if first == last {
        output.clock(first)
    } else {
        format!("{}–{}", output.clock(first), output.clock(last))
    };
    let summary = i_cal_component_get_summary(component);
    let title = if summary.is_null() {
        "Untitled event".to_owned()
    } else {
        CStr::from_ptr(summary).to_string_lossy().into_owned()
    };
    output.events.push((first, last, Event { title, time }));
    1
}

unsafe fn refresh(
    calendars: &BTreeMap<String, Calendar>,
    deadline: &Deadline,
    previous: &mut Option<(Date, Vec<Event>)>,
    selected: Option<Date>,
) -> (Date, TimeZone) {
    let now = Zoned::now();
    let date = selected.unwrap_or_else(|| now.date());
    let zone = now.time_zone().clone();
    let start = date.to_zoned(zone.clone()).unwrap().timestamp().as_second();
    let end = date
        .tomorrow()
        .unwrap()
        .to_zoned(zone.clone())
        .unwrap()
        .timestamp()
        .as_second();
    let tzid = CString::new(zone.iana_name().unwrap_or_else(|| fail())).unwrap();
    let native_zone = i_cal_timezone_get_builtin_timezone(tzid.as_ptr());
    if native_zone.is_null() {
        fail();
    }
    let mut output = Collection {
        date,
        start,
        end,
        zone: zone.clone(),
        native_zone,
        events: Vec::new(),
    };
    let utc = |stamp| {
        jiff::Timestamp::from_second(stamp)
            .unwrap()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string()
    };
    let expression = CString::new(format!(
        "(occur-in-time-range? (make-time \"{}\") (make-time \"{}\"))",
        utc(start - 86400),
        utc(end + 86400)
    ))
    .unwrap();
    for calendar in calendars.values() {
        deadline.call(|| {
            e_cal_client_set_default_timezone(calendar.client.0, native_zone);
            let mut components = ptr::null_mut();
            let mut error = ptr::null_mut();
            if e_cal_client_get_object_list_sync(
                calendar.client.0,
                expression.as_ptr(),
                &mut components,
                ptr::null_mut(),
                &mut error,
            ) == 0
            {
                check(error);
                fail();
            }
            let any = !components.is_null();
            g_slist_free_full(components, Some(g_object_unref));
            if any {
                e_cal_client_generate_instances_sync(
                    calendar.client.0,
                    start - 86400,
                    end + 86400,
                    ptr::null_mut(),
                    Some(collect),
                    (&mut output as *mut Collection).cast(),
                );
            }
        });
    }
    if (selected.is_none() && Zoned::now().date() != date) || TimeZone::system() != zone {
        return (date, zone);
    }
    output
        .events
        .sort_by(|a, b| (&a.0, &a.1, &a.2.title).cmp(&(&b.0, &b.1, &b.2.title)));
    let events: Vec<_> = output
        .events
        .into_iter()
        .map(|(_, _, event)| event)
        .collect();
    if previous
        .as_ref()
        .is_some_and(|(day, old)| *day == date && *old == events)
    {
        return (date, zone);
    }
    #[derive(Serialize)]
    struct Reply<'a> {
        date: String,
        events: &'a [Event],
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(
        &mut stdout,
        &Reply {
            date: date.to_string(),
            events: &events,
        },
    )
    .unwrap();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();
    *previous = Some((date, events));
    (date, zone)
}

fn main() {
    let selected = std::env::args().nth(1).map(|value| {
        value.parse::<Date>().unwrap_or_else(|_| {
            eprintln!("Usage: cosmicbar-calendar [YYYY-MM-DD]");
            std::process::exit(2);
        })
    });
    // All FFI objects and callbacks stay on this process's GLib main thread.
    unsafe {
        let deadline = Deadline::new();
        let changes = Changes::default();
        let registry = deadline.call(|| {
            let mut error = ptr::null_mut();
            let registry = Object(e_source_registry_new_sync(ptr::null_mut(), &mut error));
            check(error);
            if registry.0.is_null() {
                fail();
            }
            registry
        });
        let bus = deadline.call(|| {
            let mut error = ptr::null_mut();
            let bus = Object(g_bus_get_sync(
                GBusType_G_BUS_TYPE_SESSION,
                ptr::null_mut(),
                &mut error,
            ));
            check(error);
            if bus.0.is_null() {
                fail();
            }
            bus
        });
        g_bus_watch_name_on_connection(
            bus.0,
            c"org.gnome.evolution.dataserver.Sources5".as_ptr(),
            0,
            None,
            Some(source_service_lost),
            ptr::null_mut(),
            None,
        );
        for name in [
            c"source-added",
            c"source-changed",
            c"source-removed",
            c"source-enabled",
            c"source-disabled",
        ] {
            signal(
                registry.0,
                name,
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                    unsafe extern "C" fn(),
                >(source_changed),
                &changes,
            );
        }
        let mut calendars = BTreeMap::new();
        reconcile(registry.0, &mut calendars, &changes, &deadline);
        // Do not wait for initial view completion: render the first read now,
        // then reconcile at the completion fence and on subsequent changes.
        let mut previous = None;
        let mut day = refresh(&calendars, &deadline, &mut previous, selected);
        clock_tick(ptr::null_mut());
        loop {
            // A query spanning midnight is discarded; retry its new day before
            // waiting for another signal, including during initial loading.
            let now = Zoned::now();
            if (selected.is_none() && day.0 != now.date()) || &day.1 != now.time_zone() {
                day = refresh(&calendars, &deadline, &mut previous, selected);
                continue;
            }
            // Wait for GLib signals; the timer only checks day/timezone changes.
            g_main_context_iteration(ptr::null_mut(), 1);
            while g_main_context_iteration(ptr::null_mut(), 0) != 0 {}
            if changes.failed.replace(false) {
                fail();
            }
            let mut dirty = changes.events.replace(false);
            if changes.sources.replace(false) {
                dirty |= reconcile(registry.0, &mut calendars, &changes, &deadline);
            }
            let now = Zoned::now();
            dirty |= (selected.is_none() && day.0 != now.date()) || &day.1 != now.time_zone();
            if dirty {
                day = refresh(&calendars, &deadline, &mut previous, selected);
            }
        }
    }
}
