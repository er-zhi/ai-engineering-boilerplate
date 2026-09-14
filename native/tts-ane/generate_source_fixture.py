"""Recreate source FP32 greedy audio/codes for decoder export, without ANE assets."""
import argparse
import json
from pathlib import Path

import torch
from qwen_tts import Qwen3TTSModel

from compare_qwen_quality import save


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--text', default="I'm sorry about the charge. I'll fix it for you.")
    args = parser.parse_args()
    if args.output.exists() or args.output.with_suffix('.wav').exists():
        parser.error('Use new report and WAV paths')
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.source), dtype=torch.float32,
        device_map='cpu', local_files_only=True, attn_implementation='eager')
    tokenizer = tts.model.speech_tokenizer
    original = tokenizer.decode
    captured = []

    def capture(items, **kwargs):
        captured.append(items[0]['audio_codes'].detach().cpu().tolist())
        return original(items, **kwargs)

    tokenizer.decode = capture
    try:
        with torch.inference_mode():
            wavs, rate = tts.generate_custom_voice(text=args.text, language='English',
                speaker='Serena', non_streaming_mode=False, do_sample=False,
                subtalker_dosample=False, max_new_tokens=100)
    finally:
        tokenizer.decode = original
    if len(captured) != 1 or not captured[0] or len(captured[0]) >= 99:
        raise RuntimeError('Expected one nonempty, untruncated source utterance')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    save(args.output.with_suffix('.wav'), wavs[0], rate)
    report = {'text':args.text, 'speaker':'Serena', 'source':str(args.source),
        'scope':'Original FP32 greedy fixture; not ANE performance',
        'chunks':[{'codes':codes} for codes in captured[0]]}
    args.output.write_text(json.dumps(report, indent=2)+'\n')
    print(f'Saved {args.output}: {len(captured[0])} frames', flush=True)


if __name__ == '__main__':
    main()
