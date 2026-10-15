//! A bare std type declares no `ToolContext` key; the diagnostic names the
//! fix (derive `ContextValue` on a newtype).

use mio_core::tool::ToolContext;

fn main() {
    let mut context = ToolContext::new();
    let _ = context.insert(String::from("session"));
}
