// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

pub mod args;
pub mod auth_tokens;
pub mod cache;
pub mod cdp;
pub mod checksum;
pub mod compat;
pub mod deno_dir;
pub mod diagnostics;
pub mod diff;
pub mod display;
pub mod emit;
pub mod errors;
pub mod file_fetcher;
pub mod file_watcher;
pub mod fmt_errors;
pub mod fs_util;
pub mod graph_util;
pub mod http_cache;
pub mod http_util;
pub mod lockfile;
pub mod logger;
pub mod module_loader;
pub mod ops;
pub mod proc_state;
pub mod resolver;
pub mod text_encoding;
pub mod tools;
pub mod tsc;
pub mod unix_util;
pub mod version;
pub mod windows_util;

use args::BenchFlags;
use args::BundleFlags;
use args::CacheFlags;
use args::CheckFlags;
use args::CompletionsFlags;
use args::CoverageFlags;
use args::DenoSubcommand;
use args::EvalFlags;
use args::Flags;
use args::RunFlags;
use args::TestFlags;
use args::TypeCheckMode;
use args::VendorFlags;
use cache::TypeCheckCache;
use emit::TsConfigType;
use file_fetcher::File;
use file_watcher::ResolutionResult;
use fmt_errors::format_js_error;
use graph_util::graph_lock_or_exit;
use graph_util::graph_valid;
use module_loader::CliModuleLoader;
use proc_state::ProcState;
use resolver::ImportMapResolver;
use resolver::JsxResolver;

use super::deno_runtime::colors;
use super::deno_runtime::ops::worker_host::CreateWebWorkerCb;
use super::deno_runtime::ops::worker_host::PreloadModuleCb;
use super::deno_runtime::permissions::Permissions;
use super::deno_runtime::web_worker::WebWorker;
use super::deno_runtime::web_worker::WebWorkerOptions;
use super::deno_runtime::worker::MainWorker;
use super::deno_runtime::worker::WorkerOptions;
use super::deno_runtime::BootstrapOptions;
use args::CliOptions;
use deno_ast::MediaType;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::future::FutureExt;
use deno_core::futures::future::LocalFutureObj;
use deno_core::futures::Future;
use deno_core::located_script_name;
use deno_core::parking_lot::RwLock;
use deno_core::resolve_url_or_path;
use deno_core::serde_json;
use deno_core::v8_set_flags;
use deno_core::Extension;
use deno_core::ModuleSpecifier;
use log::debug;
use log::info;
use std::env;
use std::io::Read;
use std::io::Write;
use std::iter::once;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

fn create_web_worker_preload_module_callback(ps: ProcState) -> Arc<PreloadModuleCb> {
    let compat = ps.options.compat();

    Arc::new(move |mut worker| {
        let fut = async move {
            if compat {
                worker.execute_side_module(&compat::GLOBAL_URL).await?;
                worker.execute_side_module(&compat::MODULE_URL).await?;
            }

            Ok(worker)
        };
        LocalFutureObj::new(Box::new(fut))
    })
}

fn create_web_worker_callback(
    ps: ProcState,
    stdio: super::deno_runtime::ops::io::Stdio,
) -> Arc<CreateWebWorkerCb> {
    Arc::new(move |args| {
        let maybe_inspector_server = ps.maybe_inspector_server.clone();

        let module_loader =
            CliModuleLoader::new_for_worker(ps.clone(), args.parent_permissions.clone());
        let create_web_worker_cb = create_web_worker_callback(ps.clone(), stdio.clone());
        let preload_module_cb = create_web_worker_preload_module_callback(ps.clone());

        let extensions = ops::cli_exts(ps.clone());

        let options = WebWorkerOptions {
            bootstrap: BootstrapOptions {
                args: ps.options.argv().clone(),
                cpu_count: std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(1),
                debug_flag: ps
                    .options
                    .log_level()
                    .map_or(false, |l| l == log::Level::Debug),
                enable_testing_features: ps.options.enable_testing_features(),
                location: Some(args.main_module.clone()),
                no_color: !colors::use_color(),
                is_tty: colors::is_tty(),
                runtime_version: version::deno(),
                ts_version: version::TYPESCRIPT.to_string(),
                unstable: ps.options.unstable(),
                user_agent: version::get_user_agent(),
            },
            extensions,
            unsafely_ignore_certificate_errors: ps
                .options
                .unsafely_ignore_certificate_errors()
                .map(ToOwned::to_owned),
            root_cert_store: Some(ps.root_cert_store.clone()),
            seed: ps.options.seed(),
            create_web_worker_cb,
            preload_module_cb,
            format_js_error_fn: Some(Arc::new(format_js_error)),
            source_map_getter: Some(Box::new(module_loader.clone())),
            module_loader,
            worker_type: args.worker_type,
            maybe_inspector_server,
            get_error_class_fn: Some(&errors::get_error_class_name),
            blob_store: ps.blob_store.clone(),
            broadcast_channel: ps.broadcast_channel.clone(),
            shared_array_buffer_store: Some(ps.shared_array_buffer_store.clone()),
            compiled_wasm_module_store: Some(ps.compiled_wasm_module_store.clone()),
            stdio: stdio.clone(),
        };

        WebWorker::bootstrap_from_options(
            args.name,
            args.permissions,
            args.main_module,
            args.worker_id,
            options,
        )
    })
}

pub fn create_main_worker(
    ps: &ProcState,
    main_module: ModuleSpecifier,
    permissions: Permissions,
    mut custom_extensions: Vec<Extension>,
    stdio: super::deno_runtime::ops::io::Stdio,
) -> MainWorker {
    let module_loader = CliModuleLoader::new(ps.clone());

    let maybe_inspector_server = ps.maybe_inspector_server.clone();
    let should_break_on_first_statement = ps.options.inspect_brk().is_some();

    let create_web_worker_cb = create_web_worker_callback(ps.clone(), stdio.clone());
    let web_worker_preload_module_cb = create_web_worker_preload_module_callback(ps.clone());

    let maybe_storage_key = ps.options.resolve_storage_key(&main_module);
    let origin_storage_dir = maybe_storage_key.map(|key| {
        ps.dir
            .root
            // TODO(@crowlKats): change to origin_data for 2.0
            .join("location_data")
            .join(checksum::gen(&[key.as_bytes()]))
    });

    let mut extensions = ops::cli_exts(ps.clone());
    extensions.append(&mut custom_extensions);

    let options = WorkerOptions {
        bootstrap: BootstrapOptions {
            args: ps.options.argv().clone(),
            cpu_count: std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1),
            debug_flag: ps
                .options
                .log_level()
                .map_or(false, |l| l == log::Level::Debug),
            enable_testing_features: ps.options.enable_testing_features(),
            location: ps.options.location_flag().map(ToOwned::to_owned),
            no_color: !colors::use_color(),
            is_tty: colors::is_tty(),
            runtime_version: version::deno(),
            ts_version: version::TYPESCRIPT.to_string(),
            unstable: ps.options.unstable(),
            user_agent: version::get_user_agent(),
        },
        extensions,
        unsafely_ignore_certificate_errors: ps
            .options
            .unsafely_ignore_certificate_errors()
            .map(ToOwned::to_owned),
        root_cert_store: Some(ps.root_cert_store.clone()),
        seed: ps.options.seed(),
        source_map_getter: Some(Box::new(module_loader.clone())),
        format_js_error_fn: Some(Arc::new(format_js_error)),
        create_web_worker_cb,
        web_worker_preload_module_cb,
        maybe_inspector_server,
        should_break_on_first_statement,
        module_loader,
        get_error_class_fn: Some(&errors::get_error_class_name),
        origin_storage_dir,
        blob_store: ps.blob_store.clone(),
        broadcast_channel: ps.broadcast_channel.clone(),
        shared_array_buffer_store: Some(ps.shared_array_buffer_store.clone()),
        compiled_wasm_module_store: Some(ps.compiled_wasm_module_store.clone()),
        stdio,
    };

    MainWorker::bootstrap_from_options(main_module, permissions, options)
}

pub fn write_to_stdout_ignore_sigpipe(bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::ErrorKind;

    match std::io::stdout().write_all(bytes) {
        Ok(()) => Ok(()),
        Err(e) => match e.kind() {
            ErrorKind::BrokenPipe => Ok(()),
            _ => Err(e),
        },
    }
}

pub fn write_json_to_stdout<T>(value: &T) -> Result<(), AnyError>
where
    T: ?Sized + serde::ser::Serialize,
{
    let mut writer = std::io::BufWriter::new(std::io::stdout());
    serde_json::to_writer_pretty(&mut writer, value)?;
    writeln!(&mut writer)?;
    Ok(())
}

pub fn get_types(unstable: bool) -> String {
    let mut types = vec![
        tsc::DENO_NS_LIB,
        tsc::DENO_CONSOLE_LIB,
        tsc::DENO_URL_LIB,
        tsc::DENO_WEB_LIB,
        tsc::DENO_FETCH_LIB,
        tsc::DENO_WEBSOCKET_LIB,
        tsc::DENO_WEBSTORAGE_LIB,
        tsc::DENO_CRYPTO_LIB,
        tsc::DENO_BROADCAST_CHANNEL_LIB,
        tsc::DENO_NET_LIB,
        tsc::SHARED_GLOBALS_LIB,
        tsc::WINDOW_LIB,
    ];

    if unstable {
        types.push(tsc::UNSTABLE_NS_LIB);
    }

    types.join("\n")
}

async fn cache_command(flags: Flags, cache_flags: CacheFlags) -> Result<i32, AnyError> {
    let ps = ProcState::build(flags).await?;
    load_and_type_check(&ps, &cache_flags.files).await?;
    ps.cache_module_emits()?;
    Ok(0)
}

async fn check_command(flags: Flags, check_flags: CheckFlags) -> Result<i32, AnyError> {
    let ps = ProcState::build(flags).await?;
    load_and_type_check(&ps, &check_flags.files).await?;
    Ok(0)
}

async fn load_and_type_check(ps: &ProcState, files: &Vec<String>) -> Result<(), AnyError> {
    let lib = ps.options.ts_type_lib_window();

    for file in files {
        let specifier = resolve_url_or_path(file)?;
        ps.prepare_module_load(
            vec![specifier],
            false,
            lib,
            Permissions::allow_all(),
            Permissions::allow_all(),
            false,
        )
        .await?;
    }

    Ok(())
}

async fn eval_command(flags: Flags, eval_flags: EvalFlags) -> Result<i32, AnyError> {
    // deno_graph works off of extensions for local files to determine the media
    // type, and so our "fake" specifier needs to have the proper extension.
    let main_module = resolve_url_or_path(&format!("./$deno$eval.{}", eval_flags.ext))?;
    let permissions = Permissions::from_options(&flags.permissions_options());
    let ps = ProcState::build(flags).await?;
    let mut worker = create_main_worker(
        &ps,
        main_module.clone(),
        permissions,
        vec![],
        Default::default(),
    );
    // Create a dummy source file.
    let source_code = if eval_flags.print {
        format!("console.log({})", eval_flags.code)
    } else {
        eval_flags.code
    }
    .into_bytes();

    let file = File {
        local: main_module.clone().to_file_path().unwrap(),
        maybe_types: None,
        media_type: MediaType::Unknown,
        source: String::from_utf8(source_code)?.into(),
        specifier: main_module.clone(),
        maybe_headers: None,
    };

    // Save our fake file into file fetcher cache
    // to allow module access by TS compiler.
    ps.file_fetcher.insert_cached(file);
    debug!("main_module {}", &main_module);
    if ps.options.compat() {
        worker.execute_side_module(&compat::GLOBAL_URL).await?;
    }
    worker.execute_main_module(&main_module).await?;
    worker.dispatch_load_event(&located_script_name!())?;
    loop {
        worker.run_event_loop(false).await?;

        if !worker.dispatch_beforeunload_event(&located_script_name!())? {
            break;
        }
    }
    worker.dispatch_unload_event(&located_script_name!())?;
    Ok(0)
}

async fn create_graph_and_maybe_check(
    root: ModuleSpecifier,
    ps: &ProcState,
    debug: bool,
) -> Result<Arc<deno_graph::ModuleGraph>, AnyError> {
    let mut cache = cache::FetchCacher::new(
        ps.emit_cache.clone(),
        ps.file_fetcher.clone(),
        Permissions::allow_all(),
        Permissions::allow_all(),
    );
    let maybe_locker = lockfile::as_maybe_locker(ps.lockfile.clone());
    let maybe_imports = ps.options.to_maybe_imports()?;
    let maybe_import_map_resolver = ps.maybe_import_map.clone().map(ImportMapResolver::new);
    let maybe_jsx_resolver = ps
        .options
        .to_maybe_jsx_import_source_module()
        .map(|im| JsxResolver::new(im, maybe_import_map_resolver.clone()));
    let maybe_resolver = if maybe_jsx_resolver.is_some() {
        maybe_jsx_resolver.as_ref().map(|jr| jr.as_resolver())
    } else {
        maybe_import_map_resolver
            .as_ref()
            .map(|im| im.as_resolver())
    };
    let graph = Arc::new(
        deno_graph::create_graph(
            vec![(root, deno_graph::ModuleKind::Esm)],
            false,
            maybe_imports,
            &mut cache,
            maybe_resolver,
            maybe_locker,
            None,
            None,
        )
        .await,
    );

    let check_js = ps.options.check_js();
    graph_valid(
        &graph,
        ps.options.type_check_mode() != TypeCheckMode::None,
        check_js,
    )?;
    graph_lock_or_exit(&graph);

    if ps.options.type_check_mode() != TypeCheckMode::None {
        let ts_config_result = ps.options.resolve_ts_config_for_emit(TsConfigType::Check {
            lib: ps.options.ts_type_lib_window(),
        })?;
        if let Some(ignored_options) = ts_config_result.maybe_ignored_options {
            eprintln!("{}", ignored_options);
        }
        let maybe_config_specifier = ps.options.maybe_config_file_specifier();
        let cache = TypeCheckCache::new(&ps.dir.type_checking_cache_db_file_path());
        let check_result = emit::check(
            &graph.roots,
            Arc::new(RwLock::new(graph.as_ref().into())),
            &cache,
            emit::CheckOptions {
                type_check_mode: ps.options.type_check_mode(),
                debug,
                maybe_config_specifier,
                ts_config: ts_config_result.ts_config,
                log_checks: true,
                reload: ps.options.reload_flag(),
            },
        )?;
        debug!("{}", check_result.stats);
        if !check_result.diagnostics.is_empty() {
            return Err(check_result.diagnostics.into());
        }
    }

    Ok(graph)
}

fn bundle_module_graph(
    graph: &deno_graph::ModuleGraph,
    ps: &ProcState,
) -> Result<deno_emit::BundleEmit, AnyError> {
    info!("{} {}", colors::green("Bundle"), graph.roots[0].0);

    let ts_config_result = ps
        .options
        .resolve_ts_config_for_emit(TsConfigType::Bundle)?;
    if ps.options.type_check_mode() == TypeCheckMode::None {
        if let Some(ignored_options) = ts_config_result.maybe_ignored_options {
            eprintln!("{}", ignored_options);
        }
    }

    deno_emit::bundle_graph(
        graph,
        deno_emit::BundleOptions {
            bundle_type: deno_emit::BundleType::Module,
            emit_options: ts_config_result.ts_config.into(),
            emit_ignore_directives: true,
        },
    )
}

async fn bundle_command(flags: Flags, bundle_flags: BundleFlags) -> Result<i32, AnyError> {
    let debug = flags.log_level == Some(log::Level::Debug);
    let cli_options = Arc::new(CliOptions::from_flags(flags)?);
    let resolver = |_| {
        let cli_options = cli_options.clone();
        let source_file1 = bundle_flags.source_file.clone();
        let source_file2 = bundle_flags.source_file.clone();
        async move {
            let module_specifier = resolve_url_or_path(&source_file1)?;

            debug!(">>>>> bundle START");
            let ps = ProcState::from_options(cli_options).await?;

            let graph = create_graph_and_maybe_check(module_specifier, &ps, debug).await?;

            let mut paths_to_watch: Vec<PathBuf> = graph
                .specifiers()
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok().and_then(|(s, _, _)| s.to_file_path().ok()))
                .collect();

            if let Ok(Some(import_map_path)) = ps
                .options
                .resolve_import_map_specifier()
                .map(|ms| ms.and_then(|ref s| s.to_file_path().ok()))
            {
                paths_to_watch.push(import_map_path);
            }

            Ok((paths_to_watch, graph, ps))
        }
        .map(move |result| match result {
            Ok((paths_to_watch, graph, ps)) => ResolutionResult::Restart {
                paths_to_watch,
                result: Ok((ps, graph)),
            },
            Err(e) => ResolutionResult::Restart {
                paths_to_watch: vec![PathBuf::from(source_file2)],
                result: Err(e),
            },
        })
    };

    let operation = |(ps, graph): (ProcState, Arc<deno_graph::ModuleGraph>)| {
        let out_file = bundle_flags.out_file.clone();
        async move {
            let bundle_output = bundle_module_graph(graph.as_ref(), &ps)?;
            debug!(">>>>> bundle END");

            if let Some(out_file) = out_file.as_ref() {
                let output_bytes = bundle_output.code.as_bytes();
                let output_len = output_bytes.len();
                fs_util::write_file(out_file, output_bytes, 0o644)?;
                info!(
                    "{} {:?} ({})",
                    colors::green("Emit"),
                    out_file,
                    colors::gray(display::human_size(output_len as f64))
                );
                if let Some(bundle_map) = bundle_output.maybe_map {
                    let map_bytes = bundle_map.as_bytes();
                    let map_len = map_bytes.len();
                    let ext = if let Some(curr_ext) = out_file.extension() {
                        format!("{}.map", curr_ext.to_string_lossy())
                    } else {
                        "map".to_string()
                    };
                    let map_out_file = out_file.with_extension(ext);
                    fs_util::write_file(&map_out_file, map_bytes, 0o644)?;
                    info!(
                        "{} {:?} ({})",
                        colors::green("Emit"),
                        map_out_file,
                        colors::gray(display::human_size(map_len as f64))
                    );
                }
            } else {
                println!("{}", bundle_output.code);
            }

            Ok(())
        }
    };

    if cli_options.watch_paths().is_some() {
        file_watcher::watch_func(
            resolver,
            operation,
            file_watcher::PrintConfig {
                job_name: "Bundle".to_string(),
                clear_screen: !cli_options.no_clear_screen(),
            },
        )
        .await?;
    } else {
        let module_graph = if let ResolutionResult::Restart { result, .. } = resolver(None).await {
            result?
        } else {
            unreachable!();
        };
        operation(module_graph).await?;
    }

    Ok(0)
}

async fn run_from_stdin(flags: Flags) -> Result<i32, AnyError> {
    let ps = ProcState::build(flags).await?;
    let main_module = resolve_url_or_path("./$deno$stdin.ts").unwrap();
    let mut worker = create_main_worker(
        &ps.clone(),
        main_module.clone(),
        Permissions::from_options(&ps.options.permissions_options()),
        vec![],
        Default::default(),
    );

    let mut source = Vec::new();
    std::io::stdin().read_to_end(&mut source)?;
    // Create a dummy source file.
    let source_file = File {
        local: main_module.clone().to_file_path().unwrap(),
        maybe_types: None,
        media_type: MediaType::TypeScript,
        source: String::from_utf8(source)?.into(),
        specifier: main_module.clone(),
        maybe_headers: None,
    };
    // Save our fake file into file fetcher cache
    // to allow module access by TS compiler
    ps.file_fetcher.insert_cached(source_file);

    debug!("main_module {}", main_module);
    if ps.options.compat() {
        worker.execute_side_module(&compat::GLOBAL_URL).await?;
    }
    worker.execute_main_module(&main_module).await?;
    worker.dispatch_load_event(&located_script_name!())?;
    loop {
        worker.run_event_loop(false).await?;
        if !worker.dispatch_beforeunload_event(&located_script_name!())? {
            break;
        }
    }
    worker.dispatch_unload_event(&located_script_name!())?;
    Ok(worker.get_exit_code())
}

// TODO(bartlomieju): this function is not handling `exit_code` set by the runtime
// code properly.
async fn run_with_watch(flags: Flags, script: String) -> Result<i32, AnyError> {
    /// The FileWatcherModuleExecutor provides module execution with safe dispatching of life-cycle events by tracking the
    /// state of any pending events and emitting accordingly on drop in the case of a future
    /// cancellation.
    struct FileWatcherModuleExecutor {
        worker: MainWorker,
        pending_unload: bool,
        compat: bool,
    }

    impl FileWatcherModuleExecutor {
        pub fn new(worker: MainWorker, compat: bool) -> FileWatcherModuleExecutor {
            FileWatcherModuleExecutor {
                worker,
                pending_unload: false,
                compat,
            }
        }

        /// Execute the given main module emitting load and unload events before and after execution
        /// respectively.
        pub async fn execute(&mut self, main_module: &ModuleSpecifier) -> Result<(), AnyError> {
            if self.compat {
                self.worker.execute_side_module(&compat::GLOBAL_URL).await?;
            }
            self.worker.execute_main_module(main_module).await?;
            self.worker.dispatch_load_event(&located_script_name!())?;
            self.pending_unload = true;

            let result = loop {
                let result = self.worker.run_event_loop(false).await;
                if !self
                    .worker
                    .dispatch_beforeunload_event(&located_script_name!())?
                {
                    break result;
                }
            };
            self.pending_unload = false;

            if let Err(err) = result {
                return Err(err);
            }

            self.worker.dispatch_unload_event(&located_script_name!())?;

            Ok(())
        }
    }

    impl Drop for FileWatcherModuleExecutor {
        fn drop(&mut self) {
            if self.pending_unload {
                self.worker
                    .dispatch_unload_event(&located_script_name!())
                    .unwrap();
            }
        }
    }

    let flags = Arc::new(flags);
    let main_module = resolve_url_or_path(&script)?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

    let operation = |(sender, main_module): (
        tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>,
        ModuleSpecifier,
    )| {
        let flags = flags.clone();
        let permissions = Permissions::from_options(&flags.permissions_options());
        async move {
            let ps = ProcState::build_for_file_watcher((*flags).clone(), sender.clone()).await?;
            // We make use an module executor guard to ensure that unload is always fired when an
            // operation is called.
            let mut executor = FileWatcherModuleExecutor::new(
                create_main_worker(
                    &ps,
                    main_module.clone(),
                    permissions,
                    vec![],
                    Default::default(),
                ),
                flags.compat,
            );

            executor.execute(&main_module).await?;

            Ok(())
        }
    };

    file_watcher::watch_func2(
        receiver,
        operation,
        (sender, main_module),
        file_watcher::PrintConfig {
            job_name: "Process".to_string(),
            clear_screen: !flags.no_clear_screen,
        },
    )
    .await?;

    Ok(0)
}

async fn run_command(flags: Flags, run_flags: RunFlags) -> Result<i32, AnyError> {
    // Read script content from stdin
    if run_flags.script == "-" {
        return run_from_stdin(flags).await;
    }

    if flags.watch.is_some() {
        return run_with_watch(flags, run_flags.script).await;
    }

    // TODO(bartlomieju): it should not be resolved here if we're in compat mode
    // because it might be a bare specifier
    // TODO(bartlomieju): actually I think it will also fail if there's an import
    // map specified and bare specifier is used on the command line - this should
    // probably call `ProcState::resolve` instead
    let main_module = resolve_url_or_path(&run_flags.script)?;
    let ps = ProcState::build(flags).await?;
    let permissions = Permissions::from_options(&ps.options.permissions_options());
    let mut worker = create_main_worker(
        &ps,
        main_module.clone(),
        permissions,
        vec![],
        Default::default(),
    );

    let mut maybe_coverage_collector = if let Some(ref coverage_dir) = ps.coverage_dir {
        let session = worker.create_inspector_session().await;

        let coverage_dir = PathBuf::from(coverage_dir);
        let mut coverage_collector = tools::coverage::CoverageCollector::new(coverage_dir, session);
        worker
            .with_event_loop(coverage_collector.start_collecting().boxed_local())
            .await?;
        Some(coverage_collector)
    } else {
        None
    };

    debug!("main_module {}", main_module);

    if ps.options.compat() {
        // TODO(bartlomieju): fix me
        assert_eq!(main_module.scheme(), "file");

        // Set up Node globals
        worker.execute_side_module(&compat::GLOBAL_URL).await?;
        // And `module` module that we'll use for checking which
        // loader to use and potentially load CJS module with.
        // This allows to skip permission check for `--allow-net`
        // which would otherwise be requested by dynamically importing
        // this file.
        worker.execute_side_module(&compat::MODULE_URL).await?;

        let use_esm_loader = compat::check_if_should_use_esm_loader(&main_module)?;

        if use_esm_loader {
            // ES module execution in Node compatiblity mode
            worker.execute_main_module(&main_module).await?;
        } else {
            // CJS module execution in Node compatiblity mode
            compat::load_cjs_module(
                &mut worker.js_runtime,
                &main_module.to_file_path().unwrap().display().to_string(),
                true,
            )?;
        }
    } else {
        // Regular ES module execution
        worker.execute_main_module(&main_module).await?;
    }

    worker.dispatch_load_event(&located_script_name!())?;

    loop {
        worker
            .run_event_loop(maybe_coverage_collector.is_none())
            .await?;
        if !worker.dispatch_beforeunload_event(&located_script_name!())? {
            break;
        }
    }

    worker.dispatch_unload_event(&located_script_name!())?;

    if let Some(coverage_collector) = maybe_coverage_collector.as_mut() {
        worker
            .with_event_loop(coverage_collector.stop_collecting().boxed_local())
            .await?;
    }
    Ok(worker.get_exit_code())
}

async fn coverage_command(flags: Flags, coverage_flags: CoverageFlags) -> Result<i32, AnyError> {
    if coverage_flags.files.is_empty() {
        return Err(generic_error("No matching coverage profiles found"));
    }

    tools::coverage::cover_files(flags, coverage_flags).await?;
    Ok(0)
}

async fn bench_command(flags: Flags, bench_flags: BenchFlags) -> Result<i32, AnyError> {
    if flags.watch.is_some() {
        tools::bench::run_benchmarks_with_watch(flags, bench_flags).await?;
    } else {
        tools::bench::run_benchmarks(flags, bench_flags).await?;
    }

    Ok(0)
}

async fn test_command(flags: Flags, test_flags: TestFlags) -> Result<i32, AnyError> {
    if let Some(ref coverage_dir) = flags.coverage_dir {
        std::fs::create_dir_all(&coverage_dir)?;
        env::set_var(
            "DENO_UNSTABLE_COVERAGE_DIR",
            PathBuf::from(coverage_dir).canonicalize()?,
        );
    }

    if flags.watch.is_some() {
        tools::test::run_tests_with_watch(flags, test_flags).await?;
    } else {
        tools::test::run_tests(flags, test_flags).await?;
    }

    Ok(0)
}

async fn completions_command(
    _flags: Flags,
    completions_flags: CompletionsFlags,
) -> Result<i32, AnyError> {
    write_to_stdout_ignore_sigpipe(&completions_flags.buf)?;
    Ok(0)
}

async fn types_command(flags: Flags) -> Result<i32, AnyError> {
    let types = get_types(flags.unstable);
    write_to_stdout_ignore_sigpipe(types.as_bytes())?;
    Ok(0)
}

async fn vendor_command(flags: Flags, vendor_flags: VendorFlags) -> Result<i32, AnyError> {
    tools::vendor::vendor(flags, vendor_flags).await?;
    Ok(0)
}

fn init_v8_flags(v8_flags: &[String]) {
    let v8_flags_includes_help = v8_flags
        .iter()
        .any(|flag| flag == "-help" || flag == "--help");
    // Keep in sync with `standalone.rs`.
    let v8_flags = once("UNUSED_BUT_NECESSARY_ARG0".to_owned())
        .chain(v8_flags.iter().cloned())
        .collect::<Vec<_>>();
    let unrecognized_v8_flags = v8_set_flags(v8_flags)
        .into_iter()
        .skip(1)
        .collect::<Vec<_>>();
    if !unrecognized_v8_flags.is_empty() {
        for f in unrecognized_v8_flags {
            eprintln!("error: V8 did not recognize flag '{}'", f);
        }
        eprintln!("\nFor a list of V8 flags, use '--v8-flags=--help'");
        std::process::exit(1);
    }
    if v8_flags_includes_help {
        std::process::exit(0);
    }
}

fn get_subcommand(flags: Flags) -> Pin<Box<dyn Future<Output = Result<i32, AnyError>>>> {
    match flags.subcommand.clone() {
        DenoSubcommand::Bench(bench_flags) => bench_command(flags, bench_flags).boxed_local(),
        DenoSubcommand::Bundle(bundle_flags) => bundle_command(flags, bundle_flags).boxed_local(),
        DenoSubcommand::Eval(eval_flags) => eval_command(flags, eval_flags).boxed_local(),
        DenoSubcommand::Cache(cache_flags) => cache_command(flags, cache_flags).boxed_local(),
        DenoSubcommand::Check(check_flags) => check_command(flags, check_flags).boxed_local(),
        DenoSubcommand::Coverage(coverage_flags) => {
            coverage_command(flags, coverage_flags).boxed_local()
        }
        DenoSubcommand::Run(run_flags) => run_command(flags, run_flags).boxed_local(),
        DenoSubcommand::Test(test_flags) => test_command(flags, test_flags).boxed_local(),
        DenoSubcommand::Completions(completions_flags) => {
            completions_command(flags, completions_flags).boxed_local()
        }
        DenoSubcommand::Types => types_command(flags).boxed_local(),
        DenoSubcommand::Vendor(vendor_flags) => vendor_command(flags, vendor_flags).boxed_local(),
        _ => unreachable!(),
    }
}

fn setup_panic_hook() {
    // This function does two things inside of the panic hook:
    // - Tokio does not exit the process when a task panics, so we define a custom
    //   panic hook to implement this behaviour.
    // - We print a message to stderr to indicate that this is a bug in Deno, and
    //   should be reported to us.
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        eprintln!("\n============================================================");
        eprintln!("Deno has panicked. This is a bug in Deno. Please report this");
        eprintln!("at https://github.com/denoland/deno/issues/new.");
        eprintln!("If you can reliably reproduce this panic, include the");
        eprintln!("reproduction steps and re-run with the RUST_BACKTRACE=1 env");
        eprintln!("var set and include the backtrace in your report.");
        eprintln!();
        eprintln!("Platform: {} {}", env::consts::OS, env::consts::ARCH);
        eprintln!("Version: {}", version::deno());
        eprintln!("Args: {:?}", env::args().collect::<Vec<_>>());
        eprintln!();
        orig_hook(panic_info);
        std::process::exit(1);
    }));
}

fn unwrap_or_exit<T>(result: Result<T, AnyError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            let error_string = match error.downcast_ref::<JsError>() {
                Some(e) => format_js_error(e),
                None => format!("{:?}", error),
            };
            eprintln!(
                "{}: {}",
                colors::red_bold("error"),
                error_string.trim_start_matches("error: ")
            );
            std::process::exit(1);
        }
    }
}
