//! Exhaustive session admission for nightly-2026-08-24. Field lists come from
//! compiler/rustc_session/src/options.rs at the pinned compiler revision.
//! Destructuring without `..` deliberately fails to compile when rustc adds flags.
use rustc_middle::ty::TyCtxt;
use rustc_session::config::{CodegenOptions, DebugInfo, Lto, OutputType, UnstableOptions};
use rustc_target::spec::TargetTuple;

fn field(bytes: &mut Vec<u8>, name: &str, value: impl std::fmt::Debug) {
    let value = format!("{value:?}");
    bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

macro_rules! options {
    ($bytes:expr, $actual:expr, $kind:ident, $prefix:literal,
     accepted [$($accepted:ident),* $(,)?], rejected [$($rejected:ident),* $(,)?]) => {{
        raw_flags($prefix.trim(), &[$(stringify!($accepted)),*])?;
        let actual = $actual;
        let defaults = $kind::default();
        let $kind { $($accepted,)* $($rejected,)* } = actual;
        $(field($bytes, concat!($prefix, stringify!($accepted)), $accepted);)*
        $(if format!("{:?}", $rejected) != format!("{:?}", defaults.$rejected) {
            return Err(format!("unsupported {}{} differs from the compiler default", $prefix, stringify!($rejected).replace('_', "-")));
        })*
    }};
}

pub fn key(tcx: TyCtxt<'_>) -> Result<Vec<u8>, String> {
    let sess = tcx.sess;
    match &sess.opts.target_triple {
        TargetTuple::TargetTuple(triple) if triple == "aarch64-apple-darwin" => {}
        _ => return Err("only built-in aarch64-apple-darwin is supported".into()),
    }
    if sess.lto() != Lto::No {
        return Err("LTO requires -C lto=off".into());
    }
    if sess.opts.cg.embed_bitcode {
        return Err("embedded bitcode requires -C embed-bitcode=no".into());
    }
    if sess.opts.cg.debuginfo != DebugInfo::None {
        return Err("debug information is unsupported".into());
    }
    if sess.opts.incremental.is_some() {
        return Err("incremental compilation is unsupported".into());
    }
    for output in sess.opts.output_types.keys() {
        if !matches!(
            output,
            OutputType::Exe | OutputType::Object | OutputType::Metadata | OutputType::DepInfo
        ) {
            return Err(format!("unsupported output type {output:?}"));
        }
    }
    let mut bytes = Vec::new();
    field(
        &mut bytes,
        "rustc-vV",
        include_str!(env!("HASH_RUSTC_VERSION_FILE")),
    );
    field(&mut bytes, "target-triple", &sess.opts.target_triple);
    field(&mut bytes, "target", &sess.target);
    field(&mut bytes, "optimize", sess.opts.optimize);
    field(&mut bytes, "debug-assertions", sess.opts.debug_assertions);
    field(&mut bytes, "overflow-checks", sess.overflow_checks());
    field(&mut bytes, "panic-strategy", sess.panic_strategy());
    field(&mut bytes, "edition", sess.opts.edition);
    field(
        &mut bytes,
        "remap-path-prefix",
        &sess.opts.remap_path_prefix,
    );
    field(&mut bytes, "remap-path-scope", sess.opts.remap_path_scope);
    for name in ["MACOSX_DEPLOYMENT_TARGET", "SDKROOT", "SOURCE_DATE_EPOCH"] {
        field(&mut bytes, name, std::env::var_os(name));
    }
    options!(&mut bytes, &sess.opts.cg, CodegenOptions, "-C ",
        accepted [
            codegen_units,
            debug_assertions,
            debuginfo,
            embed_bitcode,
            extra_filename,
            force_frame_pointers,
            jump_tables,
            linker,
            lto,
            metadata,
            no_redzone,
            no_vectorize_loops,
            no_vectorize_slp,
            opt_level,
            overflow_checks,
            panic,
            relocation_model,
            symbol_mangling_version,
        ], rejected [
            ar,
            code_model,
            collapse_macro_debuginfo,
            control_flow_guard,
            default_linker_libraries,
            dlltool,
            dwarf_version,
            force_unwind_tables,
            help,
            incremental,
            inline_threshold,
            instrument_coverage,
            link_arg,
            link_args,
            link_dead_code,
            link_self_contained,
            linker_features,
            linker_flavor,
            linker_plugin_lto,
            llvm_args,
            no_prepopulate_passes,
            no_stack_check,
            passes,
            prefer_dynamic,
            profile_generate,
            profile_sample_use,
            profile_use,
            relro_level,
            remark,
            rpath,
            save_temps,
            soft_float,
            split_debuginfo,
            strip,
            target_cpu,
            target_feature,
            unsafe_allow_abi_mismatch,
        ]
    );
    options!(&mut bytes, &sess.opts.unstable_opts, UnstableOptions, "-Z ",
        accepted [embed_metadata], rejected [
            allow_features,
            allow_partial_mitigations,
            always_encode_mir,
            annotate_moves,
            assert_incr_state,
            assume_incomplete_release,
            assumptions_on_binders,
            autodiff,
            autodiff_post_passes,
            binary_dep_depinfo,
            box_noalias,
            branch_protection,
            build_sdylib_interface,
            cache_proc_macros,
            cf_protection,
            check_cfg_all_expected,
            checksum_hash_algorithm,
            codegen_backend,
            codegen_emit_retag,
            codegen_source_order,
            contract_checks,
            coverage_options,
            crate_attr,
            cross_crate_inline_threshold,
            debug_info_type_line_numbers,
            debuginfo_compression,
            debuginfo_for_profiling,
            deduplicate_diagnostics,
            default_visibility,
            deny_partial_mitigations,
            dep_info_omit_d_target,
            direct_access_external_data,
            disable_fast_paths,
            disable_incr_comp_backend_caching,
            disable_param_env_normalization_hack,
            dual_proc_macros,
            dump_dep_graph,
            dump_mir,
            dump_mir_dataflow,
            dump_mir_dir,
            dump_mir_exclude_alloc_bytes,
            dump_mir_exclude_pass_number,
            dump_mir_graphviz,
            dump_mono_stats,
            dump_mono_stats_format,
            dwarf_version,
            dylib_lto,
            eagerly_emit_delayed_bugs,
            ehcont_guard,
            embed_source,
            emit_stack_sizes,
            enforce_type_length_limit,
            experimental_default_bounds,
            export_executable_symbols,
            external_clangrt,
            extra_const_ub_checks,
            fewer_names,
            fixed_x18,
            flatten_format_args,
            fmt_debug,
            force_intrinsic_fallback,
            force_unstable_if_unmarked,
            function_return,
            function_sections,
            future_incompat_test,
            graphviz_dark_mode,
            graphviz_font,
            has_thread_local,
            help,
            higher_ranked_assumptions,
            hint_mostly_unused,
            hint_msrv,
            human_readable_cgu_names,
            identify_regions,
            ignore_directory_in_diagnostics_source_blocks,
            implicit_sysroot_deps,
            incremental_ignore_spans,
            incremental_info,
            incremental_verify_ich,
            indirect_branch_cs_prefix,
            inline_llvm,
            inline_mir,
            inline_mir_forwarder_threshold,
            inline_mir_hint_threshold,
            inline_mir_preserve_debug,
            inline_mir_threshold,
            input_stats,
            instrument_mcount,
            instrument_xray,
            internal_testing_features,
            large_data_threshold,
            layout_seed,
            link_directives,
            link_native_libraries,
            link_only,
            lint_llvm_ir,
            lint_mir,
            llvm_module_flag,
            llvm_plugins,
            llvm_target_feature,
            llvm_time_trace,
            llvm_writable,
            location_detail,
            ls,
            macro_backtrace,
            macro_stats,
            maximal_hir_to_mir_coverage,
            merge_functions,
            meta_stats,
            metrics_dir,
            min_function_alignment,
            min_recursion_limit,
            mir_enable_passes,
            mir_include_spans,
            mir_opt_bisect_limit,
            mir_opt_level,
            mir_preserve_ub,
            mir_strip_debuginfo,
            move_size_limit,
            namespaced_crates,
            next_solver,
            nll_facts,
            nll_facts_dir,
            no_analysis,
            no_codegen,
            no_generate_arange_section,
            no_implied_bounds_compat,
            no_leak_check,
            no_link,
            no_parallel_backend,
            no_profiler_runtime,
            no_steal_thir,
            no_trait_vptr,
            no_unique_section_names,
            normalize_docs,
            offload,
            on_broken_pipe,
            osx_rpath_install_name,
            packed_bundled_libs,
            packed_stack,
            panic_abort_tests,
            panic_in_drop,
            parse_crate_root_only,
            patchable_function_entry,
            plt,
            pointer_authentication,
            polonius,
            pre_link_arg,
            pre_link_args,
            precise_enum_drop_elaboration,
            print_codegen_stats,
            print_codegen_stats_json,
            print_llvm_passes,
            print_mono_items,
            print_type_sizes,
            proc_macro_backtrace,
            proc_macro_execution_strategy,
            profile_closures,
            profiler_runtime,
            query_dep_graph,
            randomize_layout,
            reg_struct_return,
            regparm,
            relax_elf_relocations,
            remap_cwd_prefix,
            remark_dir,
            renormalize_rigid_aliases,
            retpoline,
            retpoline_external_thunk,
            sanitizer,
            sanitizer_cfi_canonical_jump_tables,
            sanitizer_cfi_generalize_pointers,
            sanitizer_cfi_normalize_integers,
            sanitizer_cfi_diag,
            sanitizer_cfi_recover,
            sanitizer_dataflow_abilist,
            sanitizer_kcfi_arity,
            sanitizer_memory_track_origins,
            sanitizer_recover,
            saturating_float_casts,
            self_profile,
            self_profile_counter,
            self_profile_events,
            share_generics,
            shell_argfiles,
            simulate_remapped_rust_src_base,
            small_data_threshold,
            span_debug,
            span_free_formats,
            split_dwarf_inlining,
            split_dwarf_kind,
            split_dwarf_out_dir,
            split_lto_unit,
            src_hash_algorithm,
            stack_protector,
            staticlib_allow_rdylib_deps,
            staticlib_hide_internal_symbols,
            staticlib_rename_internal_symbols,
            staticlib_prefer_dynamic,
            strict_init_checks,
            teach,
            temps_dir,
            terminal_urls,
            thinlto,
            threads,
            time_llvm_passes,
            time_passes,
            time_passes_format,
            tiny_const_eval_limit,
            tls_model,
            trace_macros,
            track_diagnostics,
            translate_remapped_path_to_local_path,
            trap_unreachable,
            treat_err_as_bug,
            trim_diagnostic_paths,
            tune_cpu,
            typing_mode_post_typeck_until_borrowck,
            ub_checks,
            ui_testing,
            uninit_const_chunk_threshold,
            unleash_the_miri_inside_of_you,
            unpretty,
            unsound_mir_opts,
            unstable_options,
            use_ctors_section,
            use_sync_unwind,
            validate_mir,
            verbose_asm,
            verbose_internals,
            verify_llvm_ir,
            virtual_function_elimination,
            wasi_exec_model,
            wasm_c_abi,
            wasm_proc_macros,
            write_long_types_to_disk,
        ]
    );
    Ok(bytes)
}

fn raw_flags(short: &str, accepted: &[&str]) -> Result<(), String> {
    let long = if short == "-C" {
        "--codegen"
    } else {
        "--unstable"
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg.starts_with('@') {
            return Err("response-file arguments are unsupported by the audited flag guard".into());
        }
        let value = if arg == short || arg == long {
            Some(args.next().ok_or("missing compiler option")?)
        } else if let Some(value) = arg.strip_prefix(&format!("{long}=")) {
            Some(value.to_owned())
        } else {
            arg.strip_prefix(short).map(str::to_owned)
        };
        if let Some(value) = value {
            let name = value.split('=').next().unwrap_or("").replace('-', "_");
            if !accepted.contains(&name.as_str()) {
                return Err(if short == "-C" {
                    format!("unsupported explicit -C {}", name.replace('_', "-"))
                } else {
                    format!("unsupported explicit unstable flag {arg}")
                });
            }
        }
    }
    Ok(())
}
