//! `run` commands (D10): any replica expands one into a task per host.
//! backend authorized the command on the resolved hosts (D12, D16).

use lapin::message::Delivery;
use regencylib::amqp::Handled;
use regencylib::messages::{Command, CommandBody, run_key};
use regencylib::selector::resolve;
use tracing::{Instrument, error, info, info_span, warn};

use crate::dispatch::{Body, Dispatcher, Plan};
use crate::state::State;

pub async fn handle(state: &State, dispatcher: &Dispatcher, delivery: Delivery) -> Handled {
    let command = match Command::from_bytes(&delivery.data) {
        Ok(command) => command,
        Err(e) => {
            error!(error = %e, "undecodable command, parked");
            return Handled::Reject;
        }
    };
    let CommandBody::Run(run) = &command.body else {
        return Handled::Ack;
    };
    let span = info_span!("run", command_id = %command.command_id, principal = %command.issued_by);
    async {
        let assignments = state.assignments();
        let body = match &run.one_shot {
            Some(one_shot) => Body::OneShot(one_shot),
            None => Body::Effective(&assignments),
        };
        let (hostdefs, inventories) = state.hosts();
        let resolution = resolve(&run.selector, &hostdefs, &inventories, run.pool.as_deref());
        for host_id in resolution.conflicting.iter().chain(&resolution.unknown) {
            warn!(host_id = %host_id, "host skipped (conflicting definitions or unknown)");
        }
        let command_id = command.command_id.clone();
        let key = move |host: &str| run_key(&command_id, host);
        let plan = Plan {
            job: run.job,
            body,
            created_at: command.issued_at,
            origin: &command.command_id,
            ttl: None,
            key: &key,
            progress: &|| {},
        };
        match dispatcher.dispatch(&resolution.targets, &plan).await {
            Ok(published) => {
                info!(published, one_shot = run.one_shot.is_some(), "run expanded");
                Handled::Ack
            }
            Err(e) => {
                // redelivered: same keys, duplicates dropped downstream
                warn!(error = %e, "run expansion failed, retried");
                Handled::Requeue
            }
        }
    }
    .instrument(span)
    .await
}
