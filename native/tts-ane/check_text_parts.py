"""Real model checks for incremental text conditioning and continuous PCM."""
import argparse
import hashlib
import json
from pathlib import Path
import wave
from voice_stream import VoiceStream


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference',type=Path)
    parser.add_argument('--gate',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Preserve previous evidence')
    config = json.loads(args.reference.read_text())
    def path(name):
        return Path(config[name]) if config.get(name) else None
    text = config['text']
    voice = VoiceStream(path('source'),path('packages'),args.gate,text,'',
        compiled_dir=Path('models/compiled-cache'),predictor_package=path('predictor_package'),
        prefill_packages=path('prefill_packages'),speaker=config['speaker'],model_prefix='qwen06',
        block_count=1,decoder_package=path('decoder_package'),experimental_history_decoder=True,
        text_projection_package=path('text_projection_package'),long_talker_package=path('long_talker_package'))
    limit = voice.capacity-10
    reference = list(voice.chunks_for_text(text,limit))
    args.output.mkdir(parents=True)
    results = []
    for width in [1,7,19]:
        parts = [text[i:i+width] for i in range(0,len(text),width)]
        received = []
        def producer():
            for part in parts:
                received.append(part)
                yield part
        stream = voice.chunks_for_text(producer(),limit,incremental=True)
        first = next(stream)
        received_at_first = len(received)
        actual = [first,*stream]
        pcm = b''.join(payload for payload,_ in actual)
        filename = args.output/f'parts-{width}.wav'
        with wave.open(str(filename),'wb') as wav:
            wav.setparams((1,2,24000,0,'NONE','not compressed'))
            wav.writeframes(pcm)
        exact = len(actual)==len(reference) and all(
            a[0]==b[0] and a[1]['codes']==b[1]['codes'] for a,b in zip(actual,reference))
        row = {'fragment_characters':width,'fragments':len(parts),'consumed_before_first_pcm':received_at_first,
            'first_pcm_before_full_input':received_at_first<len(parts),
            'first_pcm_ms':first[1]['request_ready_ms'],'frames':len(actual),
            'source_whole_text_pcm_and_codes_exact':exact,'status':dict(voice.last_status),
            'sha256_pcm':hashlib.sha256(pcm).hexdigest(),'wav':str(filename)}
        results.append(row)
        print(json.dumps(row),flush=True)
    report = {'scope':'Synchronous incremental-text Python API; real ANE-admitted audio generation',
        'reference':str(args.reference),'cases':results,'quality_approved':False,
        'limitations':['Not incremental network transport','No p95 claim from three runs',
            'Producer waits are synchronous; production cancellation while waiting needs a cancellable transport']}
    (args.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')


if __name__ == '__main__':
    main()
