"""Bounded parallel execution for file-scoped artifact validators."""

from concurrent.futures import ThreadPoolExecutor


MAX_VALIDATOR_WORKERS = 4


def run_file_validators(specs, workdir, output, expand_command, run_one,
                        logger, label):
    """Run file validators in parallel and return results in declaration order."""
    commands = _file_commands(
        specs, workdir, output, expand_command, logger, label,
    )
    results = _run_in_order(commands, workdir, run_one)
    return [
        (spec, command, result)
        for (spec, command), result in zip(commands, results)
    ]


def _file_commands(specs, workdir, output, expand_command, logger, label):
    commands = []
    for spec in specs:
        if spec.scope == "repo":
            logger.info(
                "%s: %s deferred (repo scope -> stage end)", label, spec.cmd,
            )
            continue
        command = expand_command(spec.cmd, workdir, str(output))
        logger.info("%s: %s", label, command)
        commands.append((spec, command))
    return commands


def _run_in_order(commands, workdir, run_one):
    if len(commands) <= 1:
        return [run_one(command, workdir) for _, command in commands]
    with ThreadPoolExecutor(
        max_workers=min(MAX_VALIDATOR_WORKERS, len(commands)),
    ) as pool:
        futures = [
            pool.submit(run_one, command, workdir)
            for _, command in commands
        ]
        return [future.result() for future in futures]
