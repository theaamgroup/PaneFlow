use super::*;
use std::ffi::OsStr;
use std::path::Path;

#[test]
fn an_absolute_home_places_all_three_roots_beneath_it() {
    let dirs = user_dirs_from(Some(OsStr::new("/tmp/paneflow-home"))).expect("resolves");
    assert_eq!(dirs.config, Path::new("/tmp/paneflow-home/config"));
    assert_eq!(dirs.data, Path::new("/tmp/paneflow-home/data"));
    assert_eq!(dirs.cache, Path::new("/tmp/paneflow-home/cache"));
    assert_eq!(dirs, user_dirs_under(Path::new("/tmp/paneflow-home")));
}

#[test]
fn the_namespace_rule_survives_the_override() {
    let dirs = user_dirs_under(Path::new("/tmp/paneflow-home"));
    assert_eq!(
        config_path_in(&dirs),
        Path::new("/tmp/paneflow-home/config")
            .join(APP_SUBDIR)
            .join("paneflow.json")
    );
    assert_eq!(
        session_path_in(&dirs),
        Path::new("/tmp/paneflow-home/config")
            .join(APP_SUBDIR)
            .join(session_filename())
    );
}

#[test]
fn unset_empty_and_relative_values_fall_back_to_the_platform_dirs() {
    let platform = user_dirs_from(None).expect("platform dirs resolve on macOS");
    assert_eq!(platform.config, dirs::config_dir().unwrap());
    assert_eq!(platform.data, dirs::data_local_dir().unwrap());
    assert_eq!(platform.cache, dirs::cache_dir().unwrap());
    assert_eq!(user_dirs_from(Some(OsStr::new(""))), Some(platform.clone()));
    assert_eq!(
        user_dirs_from(Some(OsStr::new("relative/home"))),
        Some(platform)
    );
    assert_eq!(home_override_from(Some(OsStr::new("relative/home"))), None);
    assert_eq!(home_override_from(Some(OsStr::new(""))), None);
    assert_eq!(home_override_from(None), None);
}

#[test]
fn the_env_name_is_the_upstream_one() {
    assert_eq!(HOME_ENV, "PANEFLOW_HOME");
}
