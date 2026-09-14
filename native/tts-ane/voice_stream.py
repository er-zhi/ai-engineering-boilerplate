"""Listening prototype: ANE-admitted Core ML graphs emit PCM16 chunks.

Text/voice preparation runs before inference. This is not a 30 ms implementation.
The default decoder recomputes a limited window. The experimental explicit-history
decoder preserves history, but has not passed full source/audio quality validation.
"""
import argparse
import gc
import hashlib
import json
import platform
import subprocess
import time
import threading
import wave
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel


def gate_report(stdout):
    """Accept one strict gate report, tolerating native diagnostic lines around it."""
    reports = []
    decoder = json.JSONDecoder()
    for offset,char in enumerate(stdout):
        if char != '{':
            continue
        try:
            value,_ = decoder.raw_decode(stdout[offset:])
        except json.JSONDecodeError:
            continue
        if isinstance(value,dict) and 'compiled_model' in value:
            reports.append(value)
    if len(reports)!=1:
        raise RuntimeError(f'Expected one ANE report; received: {stdout[:1000]!r}')
    report = reports[0]
    if (report.get('status')!='PASS' or report.get('ane_operation_ratio')!=1
        or report.get('cpu_preferred_operations')!=0 or report.get('gpu_preferred_operations')!=0):
        raise RuntimeError('Strict ANE admission failed')
    return report


def penalize_repetitions(scores,history,penalty):
    if not np.isfinite(penalty) or penalty <= 0:
        raise ValueError('Repetition penalty must be finite and positive')
    result = scores.copy()
    if history:
        indices = np.unique(history)
        values = result[indices]
        result[indices] = np.where(values < 0,values*penalty,values/penalty)
    return result


def decoder_window(frame):
    if frame < 0:
        raise ValueError('Negative frame')
    width = min(32,((frame//4)+1)*4)
    return width,min(frame,31)-(width-4)


def compiled_cache_path(package,directory):
    package = package.resolve(strict=True)
    digest = hashlib.sha256((platform.platform()+ct.__version__).encode())
    for path in sorted(package.rglob('*')) if package.is_dir() else [package]:
        if path.is_file():
            digest.update(str(path.relative_to(package)).encode())
            with path.open('rb') as source:
                digest.update(hashlib.file_digest(source,'sha256').digest())
    return directory/f'{package.stem}-{digest.hexdigest()[:20]}.mlmodelc'


def prepare(path,text,instruction,speaker=None,history_decoder=False):
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(path),dtype=torch.float32,device_map='cpu',
        local_files_only=True,attn_implementation='eager')
    talker = tts.model.talker
    captured = {}
    class StopCapture(Exception):
        pass
    original = talker.generate
    def capture(**kwargs):
        captured.update(kwargs)
        raise StopCapture()
    talker.generate = capture
    try:
        if tts.model.tts_model_type == 'custom_voice':
            if not speaker:
                raise ValueError('CustomVoice requires an explicit speaker')
            tts.generate_custom_voice(text=text,language='English',speaker=speaker,non_streaming_mode=False)
        else:
            tts.generate_voice_design(text=text,language='English',instruct=instruction,non_streaming_mode=False)
    except StopCapture:
        pass
    finally:
        talker.generate = original
    arrays = {name:captured[name].detach().numpy().copy() for name in
        ['inputs_embeds','trailing_text_hidden','tts_pad_embed']}
    arrays['repetition_penalty'] = float(captured.get('repetition_penalty',1.0))
    arrays['min_new_tokens'] = int(captured.get('min_new_tokens',0))
    if not np.isfinite(arrays['repetition_penalty']) or arrays['repetition_penalty'] <= 0:
        raise ValueError('Invalid source repetition penalty')
    if arrays['inputs_embeds'].shape[1]+64>128:
        raise ValueError('Instruction prefix exceeds prototype capacity')
    embeddings = [talker.get_input_embeddings().weight.detach().numpy().copy()]
    embeddings += [layer.weight.detach().numpy().copy() for layer in talker.code_predictor.get_input_embeddings()]
    quantizer = tts.model.speech_tokenizer.model.decoder.quantizer
    lookups = []
    with torch.inference_mode():
        for rvq in [quantizer.rvq_first,quantizer.rvq_rest]:
            for layer in rvq.vq.layers:
                table = rvq.output_proj(layer.decode(torch.arange(rvq.bins).view(1,-1)))
                lookups.append(table[0].T.numpy().copy())
        control_groups = []
        for model,capacity,multimodal in [(talker.model,128,True),(talker.code_predictor.model,16,False)]:
            controls = []
            for pos in range(capacity):
                write = np.zeros((1,1,capacity,1),np.float32)
                write[:,:,pos,:] = 1
                mask = np.full((1,1,1,capacity),-np.inf,np.float32)
                mask[...,:pos+1] = 0
                positions = torch.full((3,1,1) if multimodal else (1,1),pos,dtype=torch.long)
                cos,sin = model.rotary_emb(torch.zeros(1,1,model.config.hidden_size),positions)
                if multimodal:
                    cos,sin = cos[0],sin[0]
                controls.append(dict(write_mask=write,attention_mask=mask,
                    cosine=cos.unsqueeze(1).numpy().copy(),sine=sin.unsqueeze(1).numpy().copy()))
            control_groups.append(controls)
        if history_decoder:
            decoder = tts.model.speech_tokenizer.model.decoder
            controls = []
            for pos in range(128):
                cos,sin = decoder.pre_transformer.rotary_emb(
                    torch.zeros(1,1,decoder.config.hidden_size),torch.tensor([[pos]]))
                mask = np.full((1,1,1,decoder.config.sliding_window),-np.inf,np.float32)
                mask[...,-min(pos+1,decoder.config.sliding_window):] = 0
                controls.append(dict(cosine=cos.unsqueeze(1).numpy().copy(),
                    sine=sin.unsqueeze(1).numpy().copy(),attention_mask=mask))
            control_groups.append(controls)
    return arrays,embeddings,lookups,control_groups,talker.config.codec_eos_token_id


class VoiceStream:
    def __init__(self,source,packages,gate,text,instruction,compiled_dir=None,predictor_package=None,prefill_packages=None,first_decoder_package=None,speaker=None,model_prefix='qwen17',block_count=4,decoder_package=None,tail_decoder_package=None,decoder_windows=None,startup_decoder_package=None,experimental_history_decoder=False,experimental_prefix_state=False,short_talker_package=None):
        if short_talker_package is not None and (not experimental_prefix_state or block_count != 1):
            raise ValueError('Short talker requires invariant prefix and one full talker block')
        if experimental_prefix_state and (prefill_packages is not None or model_prefix != 'qwen06' or not speaker):
            raise ValueError('Prefix-state experiment requires Qwen06 CustomVoice without batched prefill')
        if experimental_history_decoder and (decoder_package is None or any(
            value is not None for value in [first_decoder_package,tail_decoder_package,decoder_windows,startup_decoder_package])):
            raise ValueError('Explicit-history experiment requires only decoder-package, without window decoders')
        self.history_decoder = experimental_history_decoder
        self.prepared = prepare(source,text,instruction,speaker,experimental_history_decoder)
        gc.collect()
        def load(name):
            path = packages/name
            print(f'Checking ANE: {name}',flush=True)
            target = path
            if compiled_dir is not None:
                compiled_dir.mkdir(parents=True,exist_ok=True)
                target = compiled_cache_path(path,compiled_dir)
                if not target.exists():
                    ct.models.utils.compile_model(str(path),destination_path=str(target))
            checked = subprocess.run([str(gate),str(target)],capture_output=True,text=True)
            if checked.returncode:
                raise RuntimeError(checked.stderr)
            report = gate_report(checked.stdout)
            compiled_path = report['compiled_model']
            model = ct.models.CompiledMLModel(compiled_path,compute_units=ct.ComputeUnit.CPU_AND_NE)
            model.source_spec = ct.utils.load_spec(str(path))
            print(f'Loaded ANE: {name}',flush=True)
            return model
        self.blocks = [load(f'{model_prefix}_cached_block{i}.mlpackage') for i in range(block_count)]
        self.short_blocks = [load(str(short_talker_package.resolve()))] if short_talker_package else None
        if self.short_blocks:
            spec = self.short_blocks[0].source_spec.description
            masks = {item.name:list(item.type.multiArrayType.shape) for item in spec.input}
            if not spec.state or masks.get('attention_mask') != [1,1,1,16] or masks.get('write_mask') != [1,1,16,1]:
                raise ValueError('Short talker must have stateful 16-position masks')
        self.predictor = load(str(predictor_package.resolve()) if predictor_package else 'qwen17_predictor.mlpackage')
        self.stateful_predictor = bool(self.predictor.source_spec.description.state)
        self.fused_predictor = {x.name for x in self.predictor.source_spec.description.input} == {'past_hidden','first_embedding'}
        if self.fused_predictor:
            shape = list(self.predictor.source_spec.description.output[0].type.multiArrayType.shape)
            if shape != [1,15]:
                raise ValueError('Fused runtime requires all 15 residual codes')
        self.decoder = load(str(decoder_package.resolve()) if decoder_package else 'qwen17_decoder32.mlpackage')
        self.decoder_state_shapes = {}
        if self.history_decoder:
            description = self.decoder.source_spec.description
            self.decoder_state_shapes = {item.name:tuple(item.type.multiArrayType.shape)
                for item in description.input if item.name.startswith('in_')}
            names = {item.name for item in description.input}
            outputs = {item.name:tuple(item.type.multiArrayType.shape) for item in description.output}
            if (not self.decoder_state_shapes or names != set(self.decoder_state_shapes)|{'latent','cosine','sine','attention_mask'}
                or outputs.get('pcm') != (1,1920) or any(outputs.get('out_'+name[3:]) != shape
                    for name,shape in self.decoder_state_shapes.items())):
                raise ValueError('Invalid explicit-history decoder contract')
        self.first_decoder = load(str(first_decoder_package.resolve())) if first_decoder_package else None
        self.first_decoder_frames = (int(next(x for x in self.first_decoder.source_spec.description.input
            if x.name == 'latent').type.multiArrayType.shape[-1]) if self.first_decoder else 0)
        self.tail_decoder = load(str(tail_decoder_package.resolve())) if tail_decoder_package else None
        self.startup_decoder = load(str(startup_decoder_package.resolve())) if startup_decoder_package else None
        self.decoder_windows = {}
        if decoder_windows:
            if self.first_decoder_frames != 4:
                raise ValueError('Windowed decoder requires the four-frame startup decoder')
            self.decoder_windows = {4:self.first_decoder}
            for width in range(8,33,4):
                self.decoder_windows[width] = load(str((decoder_windows/f'decoder{width}.mlpackage').resolve()))
        self.prefill_blocks = ([load(str((prefill_packages/f'{model_prefix}_prefill_block{i}.mlpackage').resolve()))
            for i in range(block_count)] if prefill_packages else None)
        self.last_status = {}
        self.predictor_samples = None
        self.prefix_states = None
        self.prefix_lock = threading.Lock()
        if experimental_prefix_state:
            prefix = self.prepared[0]['inputs_embeds']
            if prefix.shape[1] != 10 or not all(block.source_spec.description.state for block in self.blocks):
                raise ValueError('Prefix-state experiment requires the verified 9+1 prompt and stateful blocks')
            self.prefix_identity = prefix[:,:9].tobytes()
            prefix_blocks = self.short_blocks or self.blocks
            self.prefix_states = [block.make_state() for block in prefix_blocks]
            for position in range(9):
                hidden = prefix[:,position:position+1]
                for block,state in zip(prefix_blocks,self.prefix_states):
                    inputs = dict(self.prepared[3][0][position],embeddings=hidden)
                    if self.short_blocks:
                        inputs['write_mask'] = inputs['write_mask'][:,:,:16,:].copy()
                        inputs['attention_mask'] = inputs['attention_mask'][...,:16].copy()
                    result = block.predict(inputs,state=state)
                    hidden = result[block.source_spec.description.output[1].name]
                    if not np.isfinite(hidden).all():
                        raise RuntimeError('Non-finite invariant prefix')

    @staticmethod
    def zero_cache(model):
        if model.source_spec.description.state:
            return model.make_state()
        shape = next(i for i in model.source_spec.description.input if i.name=='keys').type.multiArrayType.shape
        return np.zeros(tuple(shape),np.float32),np.zeros(tuple(shape),np.float32)

    def verify_windows(self,chunks):
        if not self.decoder_windows:
            raise ValueError('No windowed decoder configured')
        lookups = self.prepared[2]
        history = np.zeros((1,512,32),np.float32)
        errors = []
        for frame,chunk in enumerate(chunks):
            index = min(frame,31)
            if frame >= 32:
                history[:,:,:-1] = history[:,:,1:].copy()
            history[0,:,index] = np.stack([table[code] for table,code in zip(lookups,chunk['codes'])]).sum(0)
            expected = self.decoder.predict({'latent':history})['pcm'][0,index*1920:(index+1)*1920]
            width,offset = decoder_window(frame)
            actual = self.decoder_windows[width].predict({'latent':history[:,:,:width].copy()})['pcm'][0,offset*1920:(offset+1)*1920]
            if frame == 0 and self.startup_decoder is not None:
                actual = self.startup_decoder.predict({'latent':history[:,:,:1].copy()})['pcm'][0]
            if actual.shape != expected.shape or not np.isfinite(actual).all():
                raise RuntimeError('Invalid window decoder output')
            errors.append(float(np.max(np.abs(actual-expected))))
        return {'scope':'Windowed vs original Core ML decoder on every generated frame; not FP32 source or listening validation',
                'frames':len(errors),'max_abs_error':max(errors),'threshold':0.02,
                'status':'PASS' if max(errors)<=0.02 else 'FAIL'}

    def chunks(self,max_frames=64):
        if self.prefix_states is None:
            yield from self._chunks(max_frames)
            return
        if not self.prefix_lock.acquire(blocking=False):
            raise RuntimeError('Prefix state is already in use; independent sessions need independent state')
        try:
            if self.prepared[0]['inputs_embeds'][:,:9].tobytes() != self.prefix_identity:
                raise RuntimeError('Prefix identity changed; rebuild the prepared voice')
            yield from self._chunks(max_frames)
        finally:
            self.prefix_lock.release()

    def _chunks(self,max_frames=64):
        started = time.perf_counter()
        if not 1 <= max_frames <= 118:
            raise ValueError('max_frames must be 1..118')
        arrays,embeddings,lookups,controls,eos = self.prepared
        # Prefill produces the first frame without a decode state. Allocate and
        # seed that state only when the second frame actually needs it.
        caches = list(self.prefix_states) if self.prefix_states is not None else [None for _ in self.blocks]
        position = 9 if self.prefix_states is not None else 0
        short_active = self.short_blocks is not None and self.prefix_states is not None
        def advance(hidden):
            nonlocal position,short_active
            if position>=128:
                raise RuntimeError('Talker KV capacity exceeded')
            if short_active and position == 16:
                for index,state in enumerate(caches):
                    caches[index] = tuple(state.read_state(name).copy() for name in ['key_cache','value_cache'])
                short_active = False
            for index,block in enumerate(self.short_blocks if short_active else self.blocks):
                inputs = dict(controls[0][position],embeddings=hidden)
                if short_active:
                    inputs['write_mask'] = inputs['write_mask'][:,:,:16,:].copy()
                    inputs['attention_mask'] = inputs['attention_mask'][...,:16].copy()
                if caches[index] is None:
                    caches[index] = self.zero_cache(block)
                elif block.source_spec.description.state and isinstance(caches[index],tuple):
                    keys,values = caches[index]
                    state = block.make_state()
                    if keys.shape[2] < 128:
                        padding = ((0,0),(0,0),(0,128-keys.shape[2]),(0,0))
                        keys,values = np.pad(keys,padding),np.pad(values,padding)
                    # The Python bridge accepts FP32 and casts into FP16 storage.
                    state.write_state('key_cache',np.ascontiguousarray(keys,dtype=np.float32))
                    state.write_state('value_cache',np.ascontiguousarray(values,dtype=np.float32))
                    caches[index] = state
                if block.source_spec.description.state:
                    result = block.predict(inputs,state=caches[index])
                else:
                    keys,values = caches[index]
                    if keys.shape[2] < 128:
                        padding = ((0,0),(0,0),(0,128-keys.shape[2]),(0,0))
                        keys,values = np.pad(keys,padding),np.pad(values,padding)
                    result = block.predict(dict(inputs,keys=keys,values=values))
                    caches[index] = result['next_keys'],result['next_values']
                output_names = [item.name for item in block.source_spec.description.output]
                hidden = result[output_names[1]]
                if not np.isfinite(hidden).all():
                    raise RuntimeError(f'Non-finite talker hidden state at position {position}, block {index}')
            position += 1
            return result['logits'],hidden
        prefix = arrays['inputs_embeds']
        if prefix.shape[1]+max_frames > 128:
            raise ValueError('Requested generation exceeds the talker KV capacity')
        if self.prefill_blocks:
            length = next(i for i in self.prefill_blocks[0].source_spec.description.input
                if i.name=='embeddings').type.multiArrayType.shape[1]
            if prefix.shape[1]>length:
                raise ValueError('Prefix exceeds exported prefill capacity')
            hidden = np.zeros((1,length,prefix.shape[2]),np.float32)
            hidden[:,:prefix.shape[1]] = prefix
            cosine = np.concatenate([controls[0][i]['cosine'] for i in range(length)],axis=2)
            sine = np.concatenate([controls[0][i]['sine'] for i in range(length)],axis=2)
            mask = np.triu(np.full((1,1,length,length),-np.inf,np.float32),k=1)
            for index,block in enumerate(self.prefill_blocks):
                result = block.predict(dict(embeddings=hidden,cosine=cosine,sine=sine,attention_mask=mask))
                hidden = result[block.source_spec.description.output[1].name]
                keys,values = result['next_keys'],result['next_values']
                if not all(np.isfinite(x).all() for x in [hidden,keys,values]):
                    raise RuntimeError(f'Non-finite prefill block {index}')
                keys[:,:,prefix.shape[1]:,:] = 0
                values[:,:,prefix.shape[1]:,:] = 0
                caches[index] = keys,values
            position = prefix.shape[1]
            logits = result['logits'][:,position-1:position]
            hidden = hidden[:,position-1:position]
        else:
            for index in range(position,prefix.shape[1]):
                logits,hidden = advance(prefix[:,index:index+1])
        prefill_ms = (time.perf_counter()-started)*1000
        latent_history = np.zeros((1,512,32),np.float32)
        decoder_state = {name:np.zeros(shape,np.float32) for name,shape in self.decoder_state_shapes.items()}
        ended = False
        semantic_history = []
        for frame in range(max_frames):
            frame_started = time.perf_counter()
            scores = penalize_repetitions(logits[0,0],semantic_history,arrays['repetition_penalty'])
            if not np.isfinite(scores).all():
                raise RuntimeError(f'Non-finite talker logits at frame {frame}')
            scores[2048:] = -np.inf
            if frame>=arrays['min_new_tokens']:
                scores[eos] = logits[0,0,eos]
            semantic = int(scores.argmax())
            if semantic==eos:
                ended = True
                break
            semantic_history.append(semantic)
            codes = [semantic]
            if self.predictor_samples is not None:
                self.predictor_samples.append({'past_hidden':hidden.copy(),
                    'first_embedding':embeddings[0][semantic].reshape(1,1,-1).copy()})
            predictor_started = time.perf_counter()
            if self.fused_predictor:
                residual = self.predictor.predict({'past_hidden':hidden,
                    'first_embedding':embeddings[0][semantic].reshape(1,1,-1)})['codes'][0]
                if (not np.isfinite(residual).all() or np.any(residual != np.floor(residual))
                    or np.any(residual < 0) or np.any(residual >= 2048)):
                    raise RuntimeError('Invalid fused residual codes')
                codes.extend(residual.astype(np.int64).tolist())
            else:
                state = self.predictor.make_state() if self.stateful_predictor else None
                if state is None:
                    keys,values = self.zero_cache(self.predictor)
                current = hidden
                for step in range(16):
                    inputs = dict(controls[1][step],embeddings=current)
                    if state is None:
                        result = self.predictor.predict(dict(inputs,keys=keys,values=values))
                        keys,values = result['next_keys'],result['next_values']
                    else:
                        result = self.predictor.predict(inputs,state=state)
                    if step==0:
                        current = embeddings[0][semantic].reshape(1,1,-1)
                    else:
                        if not np.isfinite(result['logits'][0,step-1]).all():
                            raise RuntimeError(f'Non-finite predictor logits at frame {frame}, step {step}')
                        code = int(result['logits'][0,step-1].argmax())
                        codes.append(code)
                        if step<15:
                            current = embeddings[step][code].reshape(1,1,-1)
            predictor_ms = (time.perf_counter()-predictor_started)*1000
            decoder_started = time.perf_counter()
            decoder_index = min(frame,31)
            if frame>=32:
                latent_history[:,:,:-1] = latent_history[:,:,1:].copy()
            latent_history[0,:,decoder_index] = np.stack([table[code] for table,code in zip(lookups,codes)]).sum(0)
            use_tail = frame >= 31 and self.tail_decoder is not None
            decoder = self.tail_decoder if use_tail else self.first_decoder if frame < self.first_decoder_frames else self.decoder
            decoder_input = latent_history
            if self.decoder_windows:
                width,output_index = decoder_window(frame)
                decoder = self.decoder_windows[width]
                decoder_input = latent_history[:,:,:width].copy()
            elif decoder is self.first_decoder:
                shape = tuple(next(x for x in decoder.source_spec.description.input if x.name == 'latent').type.multiArrayType.shape)
                if len(shape) != 3 or shape[:2] != (1,512) or not 1 <= shape[2] <= 32:
                    raise ValueError('Unsupported first decoder input contract')
                decoder_input = latent_history[:,:,:shape[2]].copy()
            if self.history_decoder:
                sample = dict(controls[2][frame],latent=latent_history[:,:,decoder_index:decoder_index+1].copy(),**decoder_state)
                decoded = self.decoder.predict(sample)
                pcm = decoded['pcm'][0]
                decoder_state = {name:decoded['out_'+name[3:]] for name in self.decoder_state_shapes}
                if any(not np.isfinite(value).all() for value in decoder_state.values()):
                    raise RuntimeError('Non-finite explicit decoder history')
            elif frame == 0 and self.startup_decoder is not None:
                pcm = self.startup_decoder.predict({'latent':latent_history[:,:,:1].copy()})['pcm'][0]
            elif self.decoder_windows:
                decoded = decoder.predict({'latent':decoder_input})['pcm']
                pcm = decoded[0,output_index*1920:(output_index+1)*1920]
            else:
                decoded = decoder.predict({'latent':decoder_input})['pcm']
                pcm = decoded[0,:1920] if use_tail else decoded[0,decoder_index*1920:(decoder_index+1)*1920]
            if not np.isfinite(pcm).all():
                raise RuntimeError('Non-finite PCM')
            payload = (np.clip(pcm,-1,1)*32767).round().astype('<i2').tobytes()
            yield payload,{'frame':frame,'ready_ms':(time.perf_counter()-started)*1000,'codes':codes,
                'prefill_ms':prefill_ms if frame==0 else 0,
                'prefix_tokens':int(prefix.shape[1]) if frame==0 else 0,
                'predictor_ms':predictor_ms,'decode_pcm_ms':(time.perf_counter()-decoder_started)*1000,
                'frame_compute_ms':(time.perf_counter()-frame_started)*1000}
            hidden = np.stack([embedding[code] for embedding,code in zip(embeddings,codes)]).sum(0).reshape(1,1,-1)
            trailing = arrays['trailing_text_hidden']
            hidden += trailing[:,frame:frame+1] if frame<trailing.shape[1] else arrays['tts_pad_embed']
            logits,hidden = advance(hidden)
        self.last_status = {'ended_by_eos':ended,'truncated':not ended,'elapsed_ms':(time.perf_counter()-started)*1000}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('packages',type=Path)
    parser.add_argument('--gate',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--text',default="I'm sorry about the charge. I'll fix it for you.")
    parser.add_argument('--instruction',default='A natural American English female voice. Start with sincere empathy, pause after charge, then sound confident and reassuring.')
    parser.add_argument('--serve',type=int,help='After the WAV test, serve fresh synthesis of this configured text on localhost')
    parser.add_argument('--compiled-dir',type=Path,help='Content-addressed compiled model cache; always rechecks compute plans, never caches audio')
    parser.add_argument('--predictor-package',type=Path,help='Explicit candidate predictor override; still requires strict ANE admission')
    parser.add_argument('--prefill-packages',type=Path,help='Candidate batched-prefill blocks with compatible KV outputs')
    parser.add_argument('--first-chunk-trials',type=int,default=0,help='Additional warm first-chunk trials on the same prepared prompt, not client latency')
    parser.add_argument('--first-decoder-package',type=Path,help='Short decoder used while the complete history fits its input window')
    parser.add_argument('--speaker',help='Required for CustomVoice; does not enable emotion instructions')
    parser.add_argument('--model-prefix',choices=['qwen17','qwen06'],default='qwen17')
    parser.add_argument('--block-count',type=int,choices=[1,4],default=4)
    parser.add_argument('--decoder-package',type=Path)
    parser.add_argument('--experimental-history-decoder',action='store_true',help='Explicit-state decoder experiment; not source/audio quality validated')
    parser.add_argument('--experimental-prefix-state',action='store_true',help='Reuse invariant 9-token CustomVoice KV prefix, not generated audio; serial experimental sessions only')
    parser.add_argument('--short-talker-package',type=Path,help='Experimental 16-position initial KV cache with migration to the full talker')
    parser.add_argument('--verify-prefix-state',action='store_true',help='Compare every PCM chunk and code against fresh sequential prefill; extra synthesis, outside timing')
    parser.add_argument('--tail-decoder-package',type=Path,help='Experimental exact-tail decoder used once the 32-frame history is full')
    parser.add_argument('--decoder-windows',type=Path,help='Four-output-frame tail graphs for input widths 8,12,...32')
    parser.add_argument('--startup-decoder-package',type=Path,help='One-frame decoder used only at frame zero, never for continuation')
    parser.add_argument('--capture-predictor-inputs',type=Path,help='Save real predictor inputs for experimental activation calibration')
    parser.add_argument('--verify-decoder-windows',action='store_true')
    parser.add_argument('--max-frames',type=int,default=64,choices=range(1,119),metavar='1..118')
    args = parser.parse_args()
    if not 0 <= args.first_chunk_trials <= 1000:
        parser.error('first-chunk-trials must be 0..1000')
    if args.verify_prefix_state and not args.experimental_prefix_state:
        parser.error('--verify-prefix-state requires --experimental-prefix-state')
    if args.output.exists() or args.output.with_suffix('.json').exists():
        parser.error('Output already exists; preserve previous audio and reports')
    voice = VoiceStream(args.source,args.packages,args.gate,args.text,args.instruction,args.compiled_dir,args.predictor_package,args.prefill_packages,args.first_decoder_package,args.speaker,args.model_prefix,args.block_count,args.decoder_package,args.tail_decoder_package,args.decoder_windows,args.startup_decoder_package,args.experimental_history_decoder,args.experimental_prefix_state,args.short_talker_package)
    if args.capture_predictor_inputs:
        if args.capture_predictor_inputs.exists():
            parser.error('Calibration output already exists')
        voice.predictor_samples = []
    args.output.parent.mkdir(parents=True,exist_ok=True)
    chunks = []
    first_payload = None
    parity_payloads = []
    with wave.open(str(args.output),'wb') as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(24000)
        for payload,metadata in voice.chunks(args.max_frames):
            if first_payload is None:
                first_payload = payload
            if args.verify_prefix_state:
                parity_payloads.append(payload)
            wav.writeframesraw(payload)
            chunks.append(metadata)
            print(json.dumps({'frame':metadata['frame'],'ready_ms':metadata['ready_ms'],'bytes':len(payload)}),flush=True)
    report = {'text':args.text,'instruction':None if args.speaker else args.instruction,'speaker':args.speaker,'source':str(args.source),'sample_rate':24000,
        'packages':str(args.packages.resolve()),
        'decoder_package':str(args.decoder_package.resolve()) if args.decoder_package else None,
        'experimental_history_decoder':args.experimental_history_decoder,
        'experimental_prefix_state':args.experimental_prefix_state,
        'short_talker_package':str(args.short_talker_package) if args.short_talker_package else None,
        'first_decoder_package':str(args.first_decoder_package.resolve()) if args.first_decoder_package else None,
        'tail_decoder_package':str(args.tail_decoder_package.resolve()) if args.tail_decoder_package else None,
        'decoder_windows':str(args.decoder_windows.resolve()) if args.decoder_windows else None,
        'startup_decoder_package':str(args.startup_decoder_package.resolve()) if args.startup_decoder_package else None,
        'predictor_package':str(args.predictor_package.resolve()) if args.predictor_package else None,
        'prefill_packages':str(args.prefill_packages.resolve()) if args.prefill_packages else None,
        'format':'pcm_s16le','chunks':chunks,'status':voice.last_status,
        'repetition_penalty':voice.prepared[0]['repetition_penalty'],
        'setup_excluded_from_timing':True,'greedy_sampling':True,'quality_reviewed':False,
        'audio_seconds':len(chunks)*0.08,'target_ms':30,'wav':str(args.output)}
    args.output.with_suffix('.json').write_text(json.dumps(report,indent=2)+'\n')
    if args.verify_prefix_state:
        saved_states,saved_status = voice.prefix_states,voice.last_status
        try:
            voice.prefix_states = None
            reference = list(voice.chunks(args.max_frames))
            equal = (len(reference) == len(parity_payloads)
                and all(payload == expected and metadata['codes'] == previous['codes']
                    for (payload,metadata),expected,previous in zip(reference,parity_payloads,chunks))
                and voice.last_status['ended_by_eos'] == saved_status['ended_by_eos'])
        finally:
            voice.prefix_states,voice.last_status = saved_states,saved_status
        parity = {'scope':'Full generated PCM/codes vs fresh sequential ANE prefill, not source-model quality',
            'frames':len(chunks),'reference_frames':len(reference),'exact_match':equal,'audio_cache_used':False}
        args.output.with_suffix('.prefix-parity.json').write_text(json.dumps(parity,indent=2)+'\n')
        print(json.dumps(parity),flush=True)
        if not equal:
            raise RuntimeError('Invariant-prefix full-stream parity failed')
    print(json.dumps(voice.last_status),flush=True)
    if args.capture_predictor_inputs:
        args.capture_predictor_inputs.parent.mkdir(parents=True,exist_ok=True)
        np.savez(args.capture_predictor_inputs,**{name:np.stack([sample[name] for sample in voice.predictor_samples])
            for name in ['past_hidden','first_embedding']})
        voice.predictor_samples = None
    if args.verify_decoder_windows:
        validation = voice.verify_windows(chunks)
        args.output.with_suffix('.decoder-parity.json').write_text(json.dumps(validation,indent=2)+'\n')
        print(json.dumps(validation),flush=True)
        if validation['status'] != 'PASS':
            raise RuntimeError('Window decoder parity failed')
    if args.first_chunk_trials:
        timings = []
        for trial in range(args.first_chunk_trials+5):
            generator = voice.chunks()
            try:
                payload,metadata = next(generator)
                if not payload:
                    raise RuntimeError('Empty benchmark payload')
                if payload != first_payload:
                    raise RuntimeError('First PCM changed after resetting the generation session')
                if trial>=5:
                    timings.append(metadata)
            finally:
                generator.close()
        benchmark = {'scope':'warm prepared fixed prompt to first PCM; excludes preparation and transport',
            'samples':len(timings),'warmup_runs':5,'audio_cache_used':False,
            'first_pcm_identical_after_session_reset':True,
            'stages_ms':{name:{'p50':float(np.percentile([t[name] for t in timings],50)),
                'p95':float(np.percentile([t[name] for t in timings],95))}
                for name in ['ready_ms','prefill_ms','predictor_ms','decode_pcm_ms']}}
        args.output.with_suffix('.benchmark.json').write_text(json.dumps(benchmark,indent=2)+'\n')
        print(json.dumps(benchmark),flush=True)
    if args.serve is not None:
        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def do_GET(self):
                if self.path != '/stream':
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header('Content-Type','audio/pcm; rate=24000; channels=1; format=s16le')
                self.send_header('Transfer-Encoding','chunked')
                self.send_header('Cache-Control','no-store')
                self.end_headers()
                try:
                    for payload,_ in voice.chunks():
                        self.wfile.write(f'{len(payload):X}\r\n'.encode('ascii')+payload+b'\r\n')
                        self.wfile.flush()
                    self.wfile.write(b'0\r\n\r\n')
                    self.wfile.flush()
                except (BrokenPipeError,ConnectionResetError):
                    self.close_connection = True
        print(f'Fresh synthesis at http://127.0.0.1:{args.serve}/stream (fixed configured text)',flush=True)
        HTTPServer(('127.0.0.1',args.serve),Handler).serve_forever()


if __name__=='__main__':
    main()
