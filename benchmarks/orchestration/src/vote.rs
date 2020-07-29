use crate::Context;
use color_eyre::{eyre::WrapErr, Report};
use tracing::instrument;
use tracing_futures::Instrument;
use tsunami::providers::aws;
use tsunami::Tsunami;

/// vote; requires at least two machines: a server and 1+ clients
#[instrument(name = "vote", skip(ctx))]
pub(crate) async fn main(ctx: Context) -> Result<(), Report> {
    crate::explore!(
        [
            (20, "skewed", 6, true, 0, true),
            (20, "skewed", 6, false, 0, true),
            (20, "uniform", 6, true, 0, true),
            (20, "uniform", 6, false, 0, true),
            (1000, "skewed", 3, true, 0, false),
            (1000, "skewed", 3, false, 0, false),
            (20, "skewed", 6, true, 256 * 1024 * 1024, true),
            (20, "skewed", 6, true, 384 * 1024 * 1024, true),
            (20, "skewed", 6, true, 512 * 1024 * 1024, true),
            (20, "skewed", 6, true, 768 * 1024 * 1024, true),
        ],
        one,
        ctx,
        false
    )
}

#[instrument(err, skip(ctx))]
pub(crate) async fn one(
    parameters: (usize, &'static str, usize, bool, usize, bool),
    loads: Option<Vec<usize>>,
    mut ctx: Context,
) -> Result<usize, Report> {
    let (write_every, distribution, nclients, partial, memlimit, join) = parameters;
    let mut last_good_target = 0;

    let mut aws = crate::launcher();
    aws.set_mode(aws::LaunchMode::on_demand());

    // try to ensure we do AWS cleanup
    let result: Result<_, Report> = try {
        tracing::info!("spinning up aws instances");
        let mut instances = vec![(
            String::from("server"),
            aws::Setup::default()
                .instance_type(&ctx.server_type)
                .ami(crate::AMI, "ubuntu")
                .availability_zone(ctx.az.clone())
                .setup(crate::noria_setup("noria-server", "noria-server")),
        )];
        for clienti in 0..nclients {
            instances.push((
                format!("client{}", clienti),
                aws::Setup::default()
                    .instance_type(&ctx.client_type)
                    .ami(crate::AMI, "ubuntu")
                    .availability_zone(ctx.az.clone())
                    .setup(crate::noria_setup("noria-applications", "vote")),
            ));
        }
        aws.spawn(instances, None)
            .await
            .wrap_err("failed to start instances")?;

        tracing::debug!("connecting");
        let vms = aws.connect_all().await?;
        let server = vms.get("server").unwrap();
        let s = &server.ssh;
        let cs: Vec<_> = (0..nclients)
            .map(|clienti| &vms.get(&format!("client{}", clienti)).unwrap().ssh)
            .collect();
        tracing::debug!("connected");

        let mut targets = if let Some(loads) = loads {
            Box::new(cliff::LoadIterator::from(loads)) as Box<dyn cliff::CliffSearch + Send>
        } else {
            Box::new(cliff::ExponentialCliffSearcher::until(100_000, 500_000))
        };
        let result: Result<(), Report> = try {
            let mut successful_target = None;
            while let Some(target) = targets.next() {
                if let Some(target) = successful_target.take() {
                    // last run succeeded at the given target
                    last_good_target = target;
                }
                successful_target = Some(target);

                if *ctx.exit.borrow() {
                    tracing::info!("exiting as instructed");
                    break;
                }

                let target_span = tracing::info_span!("target", target);
                async {
                    tracing::info!("start benchmark target");
                    let mut backend = if partial { "partial" } else { "full" }.to_string();
                    if !join {
                        backend.push_str("_nj");
                    }
                    let prefix = format!(
                        "{}.5000000a.{}t.{}r.{}c.{}m.{}",
                        backend, target, write_every, nclients, memlimit, distribution,
                    );

                    tracing::trace!("starting noria server");
                    let mut noria_server = crate::server::build(s, server);
                    if !partial {
                        noria_server.arg("--no-partial");
                    }
                    let noria_server = noria_server
                        .arg("--durability=memory")
                        .arg("--no-reuse")
                        .arg("--shards=0")
                        .arg("-m")
                        .arg(memlimit.to_string())
                        .spawn()
                        .wrap_err("failed to start noria-server")?;

                    crate::invoke::vote::run(
                        &prefix,
                        target,
                        distribution,
                        write_every,
                        || {
                            targets.overloaded();
                            successful_target.take();
                        },
                        &cs[..],
                        &server,
                        crate::invoke::vote::Backend::Netsoup { join },
                        &mut ctx,
                    )
                    .await?;

                    tracing::debug!("stopping server");
                    crate::server::stop(s, noria_server).await?;
                    tracing::trace!("server stopped");

                    Ok::<_, Report>(())
                }
                .instrument(target_span)
                .await?;
            }
        };

        tracing::debug!("cleaning up");
        tracing::trace!("cleaning up ssh connections");
        for (name, host) in vms {
            let host_span = tracing::trace_span!("ssh_close", name = &*name);
            async {
                tracing::trace!("closing connection");
                if let Err(e) = host.ssh.close().await {
                    tracing::warn!("ssh connection failed: {:?}", e);
                }
            }
            .instrument(host_span)
            .await
        }

        result?
    };

    tracing::trace!("cleaning up instances");
    let cleanup = aws.terminate_all().await;
    tracing::debug!("done");
    let _ = result?;
    let _ = cleanup.wrap_err("cleanup failed")?;
    Ok(last_good_target)
}
