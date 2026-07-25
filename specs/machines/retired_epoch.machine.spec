machine RetiredEpoch

state Retired initial
state Draining
state Drained terminal

event BeginDrain
event DrainCompleted

transition RETIRED_EPOCH_RETIRED_BEGIN_DRAIN Retired BeginDrain Draining
transition RETIRED_EPOCH_DRAINING_COMPLETED Draining DrainCompleted Drained

end
