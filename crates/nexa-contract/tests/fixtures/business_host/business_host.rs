pub struct BusinessHostV1;

impl GameHost for BusinessHostV1 {
    fn update(
        &mut self,
        _context: &mut nexa_runtime::ResourceContext<'_>,
        entity: i32,
        delta: i32,
    ) -> Result<i32, HostError> {
        Ok(entity + delta)
    }

    fn animation(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
        _entity: i32,
    ) -> Result<nexa_runtime::HostRequestHandle, HostError> {
        context
            .create_request()
            .map(|pending| pending.request)
            .map_err(|error| HostError(error.to_string()))
    }

    fn lock(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
        _entity: i32,
    ) -> Result<ActionLockToken, HostError> {
        let token = context
            .create_token(
                ActionLockToken::CONTENT_TYPE_ID,
                nexa_runtime::RuntimeHostDomain::Render,
            )
            .map_err(|error| HostError(error.to_string()))?;
        ActionLockToken::try_from_raw(token)
            .map_err(|error| HostError(format!("{error:?}")))
    }

    fn view(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<EnemyViewSnapshot, HostError> {
        let encoded = EnemyViewSnapshotEncoder::encode(&EnemyView { health: 40 })?;
        let handle = context
            .create_typed_snapshot(encoded)
            .map_err(|error| HostError(error.to_string()))?;
        EnemyViewSnapshot::try_from_raw(handle)
            .map_err(|error| HostError(format!("{error:?}")))
    }

    fn inspect(
        &mut self,
        _context: &mut nexa_runtime::ResourceContext<'_>,
        entity: Entity,
    ) -> Result<i32, HostError> {
        Ok(entity.0 as i32)
    }

    fn score<'a>(
        &mut self,
        _context: &mut nexa_runtime::ResourceContext<'_>,
        view: EnemyViewRef<'a>,
    ) -> Result<i32, HostError> {
        view.health().map_err(|error| HostError(format!("{error:?}")))
    }

    fn classify<'a>(
        &mut self,
        _context: &mut nexa_runtime::ResourceContext<'_>,
        error: AnimationErrorRef<'a>,
    ) -> Result<i32, HostError> {
        Ok(match error {
            AnimationErrorRef::MissingClip => 1,
            AnimationErrorRef::Code(code) => code,
            AnimationErrorRef::Cancelled => -1,
            AnimationErrorRef::Abandoned => -2,
            AnimationErrorRef::__Lifetime(_) => 0,
        })
    }

    fn heartbeat(
        &mut self,
        _context: &mut nexa_runtime::ResourceContext<'_>,
        value: i32,
    ) -> Result<i32, HostError> {
        Ok(value + 1)
    }
}
