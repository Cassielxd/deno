// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

pub mod args;
pub mod auth_tokens;
pub mod cache;
pub mod cdp;
pub mod emit;
pub mod errors;
pub mod factory;
pub mod file_fetcher;
pub mod graph_container;
pub mod graph_util;
pub mod http_util;
pub mod js;
pub mod jsr;
pub mod lsp;
pub mod module_loader;
pub mod napi;
pub mod node;
pub mod npm;
pub mod ops;
pub mod resolver;
pub mod shared;
pub mod standalone;
pub mod task_runner;
pub mod tools;
pub mod tsc;
pub mod util;
pub mod version;
pub mod worker;

use crate::args::flags_from_vec;
use crate::args::DenoSubcommand;
use crate::args::Flags;
use crate::args::DENO_FUTURE;
use crate::util::display;
use crate::util::v8::get_v8_flags_from_env;
use crate::util::v8::init_v8_flags;


pub use deno_runtime::UNSTABLE_GRANULAR_FLAGS;

use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::FutureExt;
use deno_core::unsync::JoinHandle;
use deno_npm::resolution::SnapshotFromLockfileError;
use deno_runtime::fmt_errors::format_js_error;
use deno_terminal::colors;
use std::env;
use std::future::Future;
use std::ops::Deref;

/// Ensures that all subcommands return an i32 exit code and an [`AnyError`] error type.
pub trait SubcommandOutput {
    fn output(self) -> Result<i32, AnyError>;
}

impl SubcommandOutput for Result<i32, AnyError> {
    fn output(self) -> Result<i32, AnyError> {
        self
    }
}

impl SubcommandOutput for Result<(), AnyError> {
    fn output(self) -> Result<i32, AnyError> {
        self.map(|_| 0)
    }
}

impl SubcommandOutput for Result<(), std::io::Error> {
    fn output(self) -> Result<i32, AnyError> {
        self.map(|_| 0).map_err(|e| e.into())
    }
}

/// Ensure that the subcommand runs in a task, rather than being directly executed. Since some of these
/// futures are very large, this prevents the stack from getting blown out from passing them by value up
/// the callchain (especially in debug mode when Rust doesn't have a chance to elide copies!).
#[inline(always)]
pub fn spawn_subcommand<F: Future<Output=T> + 'static, T: SubcommandOutput>(
    f: F,
) -> JoinHandle<Result<i32, AnyError>> {
    // the boxed_local() is important in order to get windows to not blow the stack in debug
    deno_core::unsync::spawn(
        async move { f.map(|r| r.output()).await }.boxed_local(),
    )
}


#[allow(clippy::print_stderr)]
pub fn setup_panic_hook() {
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
        eprintln!("Version: {}", version::DENO_VERSION_INFO.deno);
        eprintln!("Args: {:?}", env::args().collect::<Vec<_>>());
        eprintln!();
        orig_hook(panic_info);
        std::process::exit(1);
    }));
}

#[allow(clippy::print_stderr)]
pub fn exit_with_message(message: &str, code: i32) -> ! {
    eprintln!(
        "{}: {}",
        colors::red_bold("error"),
        message.trim_start_matches("error: ")
    );
    std::process::exit(code);
}

pub fn exit_for_error(error: AnyError) -> ! {
    let mut error_string = format!("{error:?}");
    let mut error_code = 1;

    if let Some(e) = error.downcast_ref::<JsError>() {
        error_string = format_js_error(e);
    } else if let Some(SnapshotFromLockfileError::IntegrityCheckFailed(e)) =
        error.downcast_ref::<SnapshotFromLockfileError>()
    {
        error_string = e.to_string();
        error_code = 10;
    }

    exit_with_message(&error_string, error_code);
}

#[allow(clippy::print_stderr)]
pub(crate) fn unstable_exit_cb(feature: &str, api_name: &str) {
    eprintln!(
        "Unstable API '{api_name}'. The `--unstable-{}` flag must be provided.",
        feature
    );
    std::process::exit(70);
}

// TODO(bartlomieju): remove when `--unstable` flag is removed.
#[allow(clippy::print_stderr)]
pub(crate) fn unstable_warn_cb(feature: &str, api_name: &str) {
    eprintln!(
        "⚠️  {}",
        colors::yellow(format!(
            "The `{}` API was used with `--unstable` flag. The `--unstable` flag is deprecated and will be removed in Deno 2.0. Use granular `--unstable-{}` instead.\nLearn more at: https://docs.deno.com/runtime/manual/tools/unstable_flags",
            api_name, feature
        ))
    );
}


fn resolve_flags_and_init(
    args: Vec<std::ffi::OsString>,
) -> Result<Flags, AnyError> {
    let flags = match flags_from_vec(args) {
        Ok(flags) => flags,
        Err(err @ clap::Error { .. })
        if err.kind() == clap::error::ErrorKind::DisplayVersion =>
            {
                // Ignore results to avoid BrokenPipe errors.
                let _ = err.print();
                std::process::exit(0);
            }
        Err(err) => exit_for_error(AnyError::from(err)),
    };

    // TODO(bartlomieju): remove when `--unstable` flag is removed.
    if flags.unstable_config.legacy_flag_enabled {
        #[allow(clippy::print_stderr)]
        if matches!(flags.subcommand, DenoSubcommand::Check(_)) {
            // can't use log crate because that's not setup yet
            eprintln!(
                "⚠️  {}",
                colors::yellow(
                    "The `--unstable` flag is not needed for `deno check` anymore."
                )
            );
        } else {
            eprintln!(
                "⚠️  {}",
                colors::yellow(
                    "The `--unstable` flag is deprecated and will be removed in Deno 2.0. Use granular `--unstable-*` flags instead.\nLearn more at: https://docs.deno.com/runtime/manual/tools/unstable_flags"
                )
            );
        }
    }

    let default_v8_flags = match flags.subcommand {
        // Using same default as VSCode:
        // https://github.com/microsoft/vscode/blob/48d4ba271686e8072fc6674137415bc80d936bc7/extensions/typescript-language-features/src/configuration/configuration.ts#L213-L214
        DenoSubcommand::Lsp => vec!["--max-old-space-size=3072".to_string()],
        _ => {
            if *DENO_FUTURE {
                // TODO(bartlomieju): I think this can be removed as it's handled by `deno_core`
                // and its settings.
                // deno_ast removes TypeScript `assert` keywords, so this flag only affects JavaScript
                // TODO(petamoriken): Need to check TypeScript `assert` keywords in deno_ast
                vec!["--no-harmony-import-assertions".to_string()]
            } else {
                vec![
                    // TODO(bartlomieju): I think this can be removed as it's handled by `deno_core`
                    // and its settings.
                    // If we're still in v1.X version we want to support import assertions.
                    // V8 12.6 unshipped the support by default, so force it by passing a
                    // flag.
                    "--harmony-import-assertions".to_string(),
                    // Verify with DENO_FUTURE for now.
                    "--no-maglev".to_string(),
                ]
            }
        }
    };

    init_v8_flags(&default_v8_flags, &flags.v8_flags, get_v8_flags_from_env());
    // TODO(bartlomieju): remove last argument in Deno 2.
    deno_core::JsRuntime::init_platform(None, !*DENO_FUTURE);
    util::logger::init(flags.log_level);

    Ok(flags)
}
