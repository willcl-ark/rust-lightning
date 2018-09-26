//! This is the config struct used to pass in configurations into the channel. This is kept by the channel manager
//! channel_limits is kept in an arc so that each channel can keep a reference to the parent one in channel manager
//! channel options is cloned into channel because those are all specified unique channel settings. It uses channel managers one as a base
use std::sync::Arc;

/// This is the main user configuration
/// This struct should contain all user customizable options as this is passed to the channel to be accessed
#[derive(Clone, Debug)]
pub struct UserConfigurations{
	/// optional user specified channel limits
	/// These are only used on startup of channels, and are referenced to a single instance
    pub channel_limits : Arc<ChannelLimits>,
	/// Channel options can change afterwords and are unique to each channel
	pub channel_options : ChannelOptions,
}

impl UserConfigurations {
	///default constructor, calls ChannelOptions and ChannelLimits constructors
    pub fn new() -> Self{
        UserConfigurations {
            channel_limits : Arc::new(ChannelLimits::new()),
			channel_options : ChannelOptions::new(),
        }
    }
}

/// This struct contains all the optional bolt 2 channel limits.
/// If the user wants to check a value, the value needs to be filled in, as by default they are not checked
#[derive(Copy, Clone, Debug)]
pub struct ChannelLimits{
	/// minimum allowed funding_satoshis
	pub funding_satoshis :u64,
	/// maximum allowed htlc_minimum_msat
	pub htlc_minimum_msat : u64,
	/// min allowed max_htlc_value_in_flight_msat
	pub max_htlc_value_in_flight_msat : u64,
	/// max allowed channel_reserve_satashis
	pub channel_reserve_satoshis : u64,
	/// min allowed max_accepted_htlcs
	pub max_accepted_htlcs : u16,
	/// min allowed dust_limit_satashis
	pub dust_limit_satoshis : u64,
	///minimum depth to a number of blocks that is considered reasonable to avoid double-spending of the funding transaction
	pub  minimum_depth : u32,
}

impl ChannelLimits {
//creating max and min possible values because if they are not set, means we should not check them.
	///default constructor creates limits so that they are not tested for
	pub fn new() -> Self{
		ChannelLimits {
			funding_satoshis : 0,
			htlc_minimum_msat : <u64>::max_value(),
			max_htlc_value_in_flight_msat : 0,
			channel_reserve_satoshis : <u64>::max_value(),
			max_accepted_htlcs : 0,
			dust_limit_satoshis : 0,
			minimum_depth : <u32>::max_value(),
		}
	}
}

/// This struct contains all the custom channel options.
#[derive(Copy, Clone, Debug)]
pub struct ChannelOptions{
	/// Amount (in millionths of a satoshi) channel will charge per transferred satoshi.
	pub fee_proportional_millionths : u32,
	///Is this channel an announced channel;
	pub announced_channel : bool,
	///do we force the incoming channel to match our announced channel preference
	pub force_announced_channel_preference : bool,
}
impl ChannelOptions {
	/// creating a struct with values.
	/// fee_proportional_millionths should be changed afterwords
	pub fn new() -> Self{
		ChannelOptions {
			fee_proportional_millionths : 0,
			announced_channel : true,
			force_announced_channel_preference : false,
		}
	}
}