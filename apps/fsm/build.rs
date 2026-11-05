// SPDX-License-Identifier: GPL-3.0-or-later

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        // The WinFsp DLL is found at run time through the registry (spike M0-1). Link arguments do not
        // cross packages, so fsm-winfsp's build.rs does not reach this one's binary and tests.
        winfsp::build::winfsp_link_delayload();

        // The testing back-end finds elements through debug info, which a release build has no use for.
        let for_tests = std::env::var("PROFILE").is_ok_and(|profile| profile == "debug");
        let config = slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .with_debug_info(for_tests);
        slint_build::compile_with_config("ui/main_window.slint", config)?;
    }
    Ok(())
}
