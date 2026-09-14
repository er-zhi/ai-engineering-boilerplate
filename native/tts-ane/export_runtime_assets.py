"""Extract the minimal host-side Serena frontend; Core ML graphs are separate."""
import argparse
import json
import shutil
from pathlib import Path
import numpy as np
from voice_stream import prepare


def save(path,array,dtype=np.float32):
    np.save(path,np.asarray(array,dtype=dtype),allow_pickle=False)


def save_embedding(path,array,dtype):
    source=np.asarray(array,dtype=np.float32)
    if dtype==np.float32:
        save(path,source)
        return
    target=np.lib.format.open_memmap(path,mode='w+',dtype=np.float16,shape=source.shape)
    indices=[]; values=[]; width=source.shape[1]
    for start in range(0,source.shape[0],512):
        stop=min(source.shape[0],start+512); block=source[start:stop]
        half=block.astype(np.float16); target[start:stop]=half
        rows,columns=np.nonzero(half.astype(np.float32)!=block)
        if len(rows):
            indices.append((rows+start)*width+columns); values.append(block[rows,columns])
    target.flush()
    if indices:
        np.savez_compressed(path.with_suffix('.corrections.npz'),
            flat_index=np.concatenate(indices).astype(np.int64),
            value=np.concatenate(values).astype(np.float32))


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('source',type=Path)
    parser.add_argument('output',type=Path)
    parser.add_argument('--capacity',type=int,default=512,choices=[128,512])
    parser.add_argument('--embedding-dtype',choices=['float16','float32'],default='float32')
    args=parser.parse_args()
    if args.output.exists():
        parser.error('Output exists')
    args.output.mkdir(parents=True)
    frontends=[]
    _,embeddings,lookups,controls,eos=prepare(args.source,'Runtime asset export.',
        '',speaker='Serena',history_decoder=True,capacity=args.capacity,frontend_sink=frontends)
    frontend=frontends[0]; talker=frontend.model.talker
    embedding_dtype=np.float16 if args.embedding_dtype=='float16' else np.float32
    save_embedding(args.output/'text_embeddings.npy',talker.get_text_embeddings().weight.detach().numpy(),embedding_dtype)
    for name,array in [('codec_embedding.npy',embeddings[0])]: save_embedding(args.output/name,array,embedding_dtype)
    for i,array in enumerate(embeddings[1:]): save_embedding(args.output/f'predictor_embedding_{i:02}.npy',array,embedding_dtype)
    for i,array in enumerate(lookups): save(args.output/f'lookup_{i:02}.npy',array)
    control_metadata=[]; control_names=[]
    names=['talker','predictor','decoder']
    for name,group in zip(names,controls):
        fields=list(group[0]); control_metadata.append({'name':name,'length':len(group),'fields':fields})
        for field in fields:
            key=f'{name}_{field}'; save(args.output/f'{key}.npy',np.stack([item[field] for item in group]))
            control_names.append(key)
    tokenizer=args.output/'tokenizer'; tokenizer.mkdir()
    for name in ['tokenizer_config.json','vocab.json','merges.txt']:
        shutil.copy2(args.source/name,tokenizer/name)
    config=json.loads((args.source/'config.json').read_text())
    metadata={'format_version':1,'upstream_model':'Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice',
        'upstream_revision':'85e237c12c027371202489a0ec509ded67b5e4b5',
        'license':'apache-2.0','speaker':'serena','speaker_id':config['talker_config']['spk_id']['serena'],
        'tts_bos_token_id':config['tts_bos_token_id'],'tts_eos_token_id':config['tts_eos_token_id'],
        'tts_pad_token_id':config['tts_pad_token_id'],'codec_eos_token_id':eos,
        'codec_prefix_ids':[config['talker_config']['codec_think_id'],
            config['talker_config']['codec_think_bos_id'],
            config['talker_config']['codec_language_id']['english'],
            config['talker_config']['codec_think_eos_id']],
        'codec_pad_id':config['talker_config']['codec_pad_id'],
        'codec_bos_id':config['talker_config']['codec_bos_id'],
        'repetition_penalty':1.05,'min_new_tokens':2,
        'embedding_dtype':args.embedding_dtype,
        'predictor_embeddings':len(embeddings)-1,'lookups':len(lookups),
        'controls':control_names,'control_groups':control_metadata}
    (args.output/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    print(json.dumps({'output':str(args.output),'files':len(list(args.output.rglob('*'))),
                      'bytes':sum(p.stat().st_size for p in args.output.rglob('*') if p.is_file())}))


if __name__=='__main__': main()
