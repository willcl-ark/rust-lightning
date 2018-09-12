#[derive(Copy, Clone)]
pub struct UserConfigurations{
    pub channel_limits : ChannelLimits,
}

impl UserConfigurations {
    pub fn new() -> Self{
        UserConfigurations {
            channel_limits : ChannelLimits::new(),
        }
    }
}

#[derive(Copy, Clone)]
pub struct ChannelLimits
{
	pub funding_satoshis :u64,
	pub htlc_minimum_msat : u64,
	pub max_htlc_value_in_flight_msat : u64,
	pub channel_reserve_satoshis : u64,
	pub max_accepted_htlcs : u16,
	pub dust_limit_satoshis : u64,
}

impl ChannelLimits {
	//creating max and min possible values because if they are not set, means we should not check them. 
	pub fn new() -> Self{
		ChannelLimits {
			funding_satoshis : 0,
			htlc_minimum_msat : <u64>::max_value(),
			max_htlc_value_in_flight_msat : 0,
			channel_reserve_satoshis : <u64>::max_value(),
			max_accepted_htlcs : 0,
			dust_limit_satoshis : 0,
		}
	}
}