//! Browser laboratory with complete Chromium, Firefox, and WebKit evidence.

pub mod artifacts;
pub mod lab;

mod browser_process;
mod browser_workload;
mod chromium;
mod firefox;
mod firefox_rdp;
#[cfg(test)]
mod test_support;
mod webkit;
