"""Controlled quality ablations; no runtime/model modifications or speed claims."""
import argparse
import json
from pathlib import Path
import time
import wave

import numpy as np
import torch
from qwen_tts import Qwen3TTSModel


def save(path,samples,sr=24000):
    samples = np.asarray(samples).reshape(-1)
    if not np.isfinite(samples).all():
        raise ValueError('Non-finite audio')
    with wave.open(str(path),'wb') as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(sr)
        output.writeframes((np.clip(samples,-1,1)*32767).round().astype('<i2').tobytes())


def difference(actual,reference):
    if actual.shape != reference.shape:
        raise ValueError('Aligned waveform comparison requires matching shapes')
    error = actual-reference
    noise = float(np.mean(error.astype(np.float64)**2))
    signal = float(np.mean(reference.astype(np.float64)**2))
    return {'peak_error':float(np.max(np.abs(error))),'rms_error':float(np.sqrt(noise)),
        'snr_db':float(10*np.log10(max(signal,1e-30)/max(noise,1e-30)))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('candidate_report',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--decode-only',action='store_true',help='Compare recorded PCM to source decoding of identical codes; no new text generation or rolling-window ablation')
    parser.add_argument('--source-fp16-control',action='store_true',help='With decode-only, also measure unchanged PyTorch FP16 decoder; CPU diagnostic, not a runtime fallback')
    args = parser.parse_args()
    if args.source_fp16_control and not args.decode_only:
        parser.error('--source-fp16-control requires --decode-only')
    if args.output.exists():
        parser.error('Use a new comparison directory')
    args.output.mkdir(parents=True)
    recorded = json.loads(args.candidate_report.read_text())
    codes = torch.tensor([frame['codes'] for frame in recorded['chunks']],dtype=torch.long)
    torch.set_num_threads(4)
    if args.decode_only:
        from export_streaming_decoder import Qwen3TTSTokenizerV2Model
        decoder = Qwen3TTSTokenizerV2Model.from_pretrained(args.source/'speech_tokenizer',
            dtype=torch.float32,local_files_only=True).eval().decoder
        with torch.inference_mode():
            reference = decoder(codes.T.unsqueeze(0))[0,0].numpy()
        save(args.output/'candidate_codes_original_decoder.wav',reference)
        with wave.open(str(args.candidate_report.with_suffix('.wav')),'rb') as wav:
            if (wav.getframerate(),wav.getsampwidth(),wav.getnchannels()) != (24000,2,1):
                raise ValueError('Expected 24 kHz mono PCM16')
            actual = np.frombuffer(wav.readframes(wav.getnframes()),dtype='<i2').astype(np.float32)/32767
        report = {'scope':'Recorded streaming PCM vs unchanged FP32 decoder on identical codes; not perceptual quality validation',
            'candidate':str(args.candidate_report),'frames':len(codes),
            'difference':difference(actual,reference),'quality_validated':False}
        if args.source_fp16_control:
            decoder = decoder.half()
            with torch.inference_mode():
                half = decoder(codes.T.unsqueeze(0))[0,0].float().numpy()
            if not np.isfinite(half).all():
                raise ValueError('Non-finite source FP16 control')
            save(args.output/'candidate_codes_source_fp16_decoder.wav',half)
            report['source_fp16_control'] = {
                'scope':'CPU numerical diagnostic only; not ANE timing or deployment',
                'vs_source_fp32':difference(half,reference),
                'candidate_vs_source_fp16':difference(actual,half)}
        args.output.joinpath('comparison.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps(report),flush=True)
        return
    print('Loading original FP32 checkpoint for controlled comparison',flush=True)
    tts = Qwen3TTSModel.from_pretrained(str(args.source),dtype=torch.float32,
        device_map='cpu',local_files_only=True,attn_implementation='eager')
    original_decode = tts.model.speech_tokenizer.decode
    captured = []
    def capture(items,**kwargs):
        captured.append(items[0]['audio_codes'].detach().cpu().tolist())
        return original_decode(items,**kwargs)
    tts.model.speech_tokenizer.decode = capture
    report = {'source':str(args.source),'candidate':str(args.candidate_report),
        'text':recorded['text'],'speaker':recorded['speaker'],'sample_rate':24000,
        'scope':'Controlled diagnostics, not a perceptual quality score or historical original reconstruction'}
    with torch.inference_mode():
        for name,sampling in [('original_greedy',False),('original_sampled_seed42',True)]:
            torch.manual_seed(42)
            captured.clear()
            started = time.perf_counter()
            options = {} if sampling else {'do_sample':False,'subtalker_dosample':False}
            wavs,sr = tts.generate_custom_voice(text=recorded['text'],language='English',
                speaker=recorded['speaker'],non_streaming_mode=False,max_new_tokens=100,**options)
            save(args.output/f'{name}.wav',wavs[0],sr)
            report[name] = {'codes':captured[0],'seconds':len(wavs[0])/sr,
                'elapsed_s':time.perf_counter()-started,'potentially_truncated':len(captured[0])>=99,
                'sampling':'checkpoint defaults, torch seed 42' if sampling else 'greedy talker and predictor'}
            print(json.dumps({'generated':name,'frames':len(captured[0]),'elapsed_s':time.perf_counter()-started}),flush=True)
        full,sr = original_decode([{'audio_codes':codes}])
        full = np.asarray(full[0]).reshape(-1)
        save(args.output/'candidate_codes_original_decoder.wav',full,sr)
        decoder = tts.model.speech_tokenizer.model.decoder
        windows = []
        for frame in range(len(codes)):
            window = codes[max(0,frame-31):frame+1].T.unsqueeze(0)
            pcm = decoder(window)[0,0,-1920:].cpu().numpy()
            windows.append(pcm)
            if frame%16 == 0:
                print(f'Original FP32 32-frame window: {frame+1}/{len(codes)}',flush=True)
        windowed = np.concatenate(windows)
        save(args.output/'candidate_codes_fp32_window32.wav',windowed)
    with wave.open(str(args.candidate_report.with_suffix('.wav')),'rb') as wav:
        actual = np.frombuffer(wav.readframes(wav.getnframes()),dtype='<i2').astype(np.float32)/32767
    report['same_codes_waveform_differences'] = {
        'optimized_vs_original_full':difference(actual,full),
        'fp32_window32_vs_original_full':difference(windowed,full),
        'optimized_vs_fp32_window32':difference(actual,windowed),
        'window32_vs_full_first_32_frames':difference(windowed[:32*1920],full[:32*1920]),
        'window32_vs_full_after_32_frames':difference(windowed[32*1920:],full[32*1920:])}
    for baseline in ['qwen06-full-w8-penalty-tail','qwen06-optimized-v9b','qwen06-fused-w8','qwen06-fused-w8-lazy-state']:
        path = args.candidate_report.parent/f'{baseline}.json'
        if path.exists():
            earlier = json.loads(path.read_text())
            previous = np.asarray([frame['codes'] for frame in earlier['chunks']])
            length = min(len(previous),len(codes))
            unequal = np.any(previous[:length] != codes.numpy()[:length],axis=1)
            indices = np.flatnonzero(unequal)
            report[baseline] = {'frames':len(previous),'first_different_frame':int(indices[0]) if len(indices) else None,
                'different_codes_in_aligned_prefix':int(np.count_nonzero(previous[:length] != codes.numpy()[:length]))}
    args.output.joinpath('comparison.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report['same_codes_waveform_differences'],indent=2),flush=True)


if __name__ == '__main__':
    main()
