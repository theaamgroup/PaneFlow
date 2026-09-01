#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
mod bindings {
    include!(env!("PANEFLOW_GHOSTTY_BINDINGS_PATH"));
}

pub use bindings::*;

pub const EXPECTED_API_VERSION: &str = env!("PANEFLOW_GHOSTTY_API_VERSION");
pub const GHOSTTY_APP_VERSION: &str = env!("PANEFLOW_GHOSTTY_APP_VERSION");
pub const GHOSTTY_XTVERSION: &str = concat!("ghostty ", env!("PANEFLOW_GHOSTTY_APP_VERSION"));

#[cfg(test)]
#[allow(dead_code)]
mod build_support;
