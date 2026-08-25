//! External URL opening helpers.

pub(crate) fn open_url(url: &str) -> std::io::Result<()> {
    open_url_impl(url)
}

fn open_url_impl(url: &str) -> std::io::Result<()> {
    open::that(url)
}
