machine ResourceToken

state Reserved initial
state Acquired
state Published
state ReleaseQueued
state Released terminal

event HostAcquire
event Publish
event EnqueueRelease
event HostRelease

resource release_record
resource host_resource

invariant nonnegative release_record
invariant nonnegative host_resource
invariant terminal_zero release_record
invariant terminal_zero host_resource

transition RESOURCE_RESERVED_HOST_ACQUIRE Acquired Publish Published
transition RESOURCE_RESERVED_ACQUIRE_ACQUIRED Reserved HostAcquire Acquired delta=release_record:+1 delta=host_resource:+1
transition RESOURCE_PUBLISHED_ENQUEUE_RELEASE Published EnqueueRelease ReleaseQueued
transition RESOURCE_RELEASE_QUEUED_HOST_RELEASED ReleaseQueued HostRelease Released delta=release_record:-1 delta=host_resource:-1

end
