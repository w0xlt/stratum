pub mod channel_factory;
pub mod proxy_group_channel;

use mining_sv2::{NewExtendedMiningJob, NewMiningJob};
use std::convert::TryInto;

/// convert extended to standard job by calculating the merkle root
pub fn extended_to_standard_job<'a>(
    extended: &NewExtendedMiningJob,
    coinbase_script: &[u8],
    channel_id: u32,
    job_id: Option<u32>,
) -> Option<NewMiningJob<'a>> {
    let merkle_root = crate::utils::merkle_root_from_path(
        extended.coinbase_tx_prefix.inner_as_ref(),
        extended.coinbase_tx_suffix.inner_as_ref(),
        coinbase_script,
        &extended.merkle_path.inner_as_ref(),
    );

    Some(NewMiningJob {
        channel_id,
        job_id: job_id.unwrap_or(extended.job_id),
        future_job: extended.future_job,
        version: extended.version,
        merkle_root: merkle_root?.try_into().ok()?,
    })
}
