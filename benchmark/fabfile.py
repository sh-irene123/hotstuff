from fabric import task
from glob import glob

from benchmark.local import LocalBench
from benchmark.logs import ParseError, LogParser
from benchmark.aggregator import LogAggregator
from benchmark.utils import Print
from benchmark.plot import Ploter, PlotError
from aws.instance import InstanceManager
from aws.remote import Bench, BenchError

# NOTE: Also requires tmux: brew install tmux


@task
def local(ctx):
    ''' Run benchmarks on localhost '''
    bench_params = {
        'nodes': 4,
        'rate': 1_000,
        'tx_size': 512,
        'duration': 20,
    }
    node_params = {
        'consensus': {
            'timeout_delay': 1_000,
            'sync_retry_delay': 10_000,
            'max_payload_size': 500,
            'min_block_delay': 0
        },
        'mempool': {
            'queue_capacity': 10_000,
            'max_payload_size': 15_000,
            'min_block_delay': 0
        }
    }
    try:
        ret = LocalBench(bench_params, node_params).run(debug=True).result()
        print(ret)
    except BenchError as e:
        Print.error(e)


@task
def create(ctx, nodes=4):
    ''' Create a testbed'''
    try:
        InstanceManager.make().create_instances(nodes)
    except BenchError as e:
        Print.error(e)


@task
def destroy(ctx):
    ''' Destroy the testbed '''
    try:
        InstanceManager.make().terminate_instances()
    except BenchError as e:
        Print.error(e)


@task
def start(ctx):
    ''' Start all machines '''
    try:
        InstanceManager.make().start_instances()
    except BenchError as e:
        Print.error(e)


@task
def stop(ctx):
    ''' Stop all machines '''
    try:
        InstanceManager.make().stop_instances()
    except BenchError as e:
        Print.error(e)


@task
def info(ctx):
    ''' Display connect information about all the available machines '''
    try:
        InstanceManager.make().print_info()
    except BenchError as e:
        Print.error(e)


@task
def install(ctx):
    ''' Install HotStuff on all machines '''
    try:
        Bench(ctx).install()
    except BenchError as e:
        Print.error(e)


@task
def remote(ctx):
    ''' Run benchmarks on AWS '''
    bench_params = {
        'nodes': [4],
        'rate': [10_000,15_000,25_000],
        'tx_size': 512,
        'duration': 300,
        'runs': 3,
    }
    node_params = {
        'consensus': {
            'timeout_delay': 30_000,
            'sync_retry_delay': 500_000,
            'max_payload_size': 1_000,
            'min_block_delay': 100
        },
        'mempool': {
            'queue_capacity': 100_000,
            'max_payload_size': 500_000,
            'min_block_delay': 100
        }
    }
    try:
        Bench(ctx).run(bench_params, node_params, debug=False)
    except BenchError as e:
        Print.error(e)


@task
def plot(ctx):
    ''' Plot performance using the logs generated by "fab remote" '''
    LogAggregator().print()
    try:
        Ploter.plot_robustness(Ploter.nodes)
        Ploter.plot_latency(Ploter.nodes)
        Ploter.plot_tps(Ploter.tx_size)
    except PlotError as e:
        Print.error(BenchError('Failed to plot performance', e))


@task
def kill(ctx):
    ''' Stop any HotStuff execution on all machines '''
    try:
        Bench(ctx).kill()
    except BenchError as e:
        Print.error(e)


@task
def logs(ctx):
    ''' Print a summary of the logs '''
    try:
        print(LogParser.process('./logs').result())
    except ParseError as e:
        Print.error(BenchError('Failed to parse logs', e))