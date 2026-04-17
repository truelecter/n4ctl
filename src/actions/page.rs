//! Page navigation actions.

use anyhow::Result;

use crate::{
    actions::{ActionContext, str_field},
    config::ActionSpec,
};

pub async fn cycle(ctx: &ActionContext, offset: isize) -> Result<()> {
    ctx.handle().cycle_page(offset).await
}

pub async fn goto(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    let name = str_field(spec, "page")?.to_string();
    ctx.handle().goto_page(&name).await
}
