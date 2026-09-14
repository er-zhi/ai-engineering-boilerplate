"""Summarize exported Instruments ANE intervals and target-process CPU samples.

Read xctrace help record/export for capture commands. These are instrumented
intervals, not uninstrumented latency benchmarks or proof of zero CPU fallback.
"""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import xml.etree.ElementTree as ET


def document(path):
    root = ET.parse(path).getroot()
    index = {item.attrib['id']:item for item in root.iter() if 'id' in item.attrib}
    def resolve(item):
        return index[item.attrib['ref']] if 'ref' in item.attrib else item
    return root,resolve


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('hardware',type=Path)
    parser.add_argument('cpu',type=Path)
    parser.add_argument('--pid',type=int,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Preserve previous evidence')
    root,resolve = document(args.hardware)
    intervals = defaultdict(list)
    starts,ends = [],[]
    for row in root.iter('row'):
        label = resolve(row.find('formatted-label')).get('fmt','')
        if not label.endswith(' Prediction') or not label.startswith(('qwen06','streaming-decoder')):
            continue
        start = int(resolve(row.find('start-time')).text)
        duration = int(resolve(row.find('duration')).text)
        intervals[label].append(duration/1e6)
        if not label.startswith('qwen06-text-projection'):
            starts.append(start)
            ends.append(start+duration)
    if not starts:
        raise ValueError('No target model ANE prediction intervals')
    begin,end = min(starts),max(ends)
    root,resolve = document(args.cpu)
    leaves = Counter()
    samples = 0
    for row in root.iter('row'):
        process = resolve(row.find('process'))
        if int(resolve(process.find('pid')).text) != args.pid:
            continue
        stamp = int(resolve(row.find('sample-time')).text)
        if not begin <= stamp <= end:
            continue
        stack_item = row.find('tagged-backtrace')
        stack = resolve(stack_item) if stack_item is not None else None
        frame = stack.find('frame') if stack is not None else None
        name = resolve(frame).get('name','unknown') if frame is not None else 'unresolved'
        leaves[name] += int(resolve(row.find('weight')).text)/1e6
        samples += 1
    report = {'scope':'Instrumented ANE prediction intervals plus CPU samples during synthesis only',
        'pid':args.pid,'synthesis_window_ms':(end-begin)/1e6,
        'ane_predictions':{label:{'count':len(values),'sum_ms':sum(values),
            'mean_ms':sum(values)/len(values),'min_ms':min(values),'max_ms':max(values)}
            for label,values in intervals.items()},
        'cpu_samples_in_window':samples,'cpu_top_leaf_sample_weights_ms':leaves.most_common(25),
        'limitations':['Not a p95 benchmark','Sample weights are not wall-clock duration',
            'ANE interval schema has no PID; model labels select the task packages',
            'CPU leaf sampling cannot prove absence of CPU model operations']}
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))


if __name__ == '__main__':
    main()
