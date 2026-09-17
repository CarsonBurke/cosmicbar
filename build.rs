fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let mut includes = Vec::new();
    for library in ["libecal-2.0", "libedataserver-1.2"] {
        let library = pkg_config::Config::new()
            .probe(library)
            .expect("install Evolution Data Server development libraries");
        includes.extend(library.include_paths);
    }
    let bindings = bindgen::Builder::default()
        .header_contents("calendar.h", "#include <libecal/libecal.h>\n#include <libedataserver/libedataserver.h>\n")
        .clang_args(includes.iter().map(|path| format!("-I{}", path.display())))
        .allowlist_function("e_source_.*|e_cal_client_.*|i_cal_(component|time|timezone)_.*|g_(object_unref|signal_connect_data|main_context_iteration|timeout_add|bus_get_sync|bus_watch_name_on_connection|error_free|list_free|slist_free_full)")
        .allowlist_var("E_CAL_CLIENT_SOURCE_TYPE_EVENTS|I_CAL_STATUS_CANCELLED")
        .derive_debug(false)
        .layout_tests(false)
        .generate()
        .expect("generate calendar bindings");
    bindings
        .write_to_file(
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("calendar.rs"),
        )
        .unwrap();
}
