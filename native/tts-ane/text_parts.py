"""Incremental English text conditioning; retain unfinished words, never restart audio state."""
from collections import deque
import re


class TextParts:
    def __init__(self,frontend,parts,capture):
        self.frontend = frontend
        self.parts = iter(parts)
        self.buffer = ''
        self.fragments = 0
        self.finished = False
        self.eos_sent = False
        self.ids = []
        self.pending = deque()
        try:
            while not self.ids:
                stable = self.read_stable()
                self.ids = self.tokenize(stable) if stable else []
                if self.finished and not self.ids:
                    raise ValueError('Text stream is empty')
            self.initial = capture(stable)
            tail = self.initial['trailing_text_hidden']
            self.eos = tail[:,-1:].copy()
            self.pad = self.initial['tts_pad_embed']
            self.pending.extend(tail[:,i:i+1].copy() for i in range(tail.shape[1]-1))
            self.initial['trailing_text_source'] = self
        except Exception:
            self.close()
            raise

    def tokenize(self,text):
        wrapped = self.frontend._build_assistant_text(text)
        return self.frontend._tokenize_texts([wrapped])[0][0,3:-5].tolist()

    def read_stable(self):
        if not self.finished:
            try:
                part = next(self.parts)
            except StopIteration:
                self.finished = True
            else:
                self.fragments += 1
                if not isinstance(part,str) or self.fragments>4096 or len(self.buffer)+len(part)>32768:
                    raise ValueError('Text stream exceeds string/fragment/character limits')
                self.buffer += part
        if self.finished:
            return self.buffer
        boundaries = list(re.finditer(r'\s+',self.buffer))
        return self.buffer[:boundaries[-1].start()] if boundaries else ''

    def next_hidden(self):
        while not self.pending and not self.finished:
            stable = self.read_stable()
            ids = self.tokenize(stable) if stable else []
            if ids[:len(self.ids)] != self.ids:
                raise ValueError('Tokenizer revised already spoken tokens; unstable text boundary')
            new_ids = ids[len(self.ids):]
            self.ids = ids
            if new_ids:
                if hasattr(self.frontend,'project_ids'):
                    projected=self.frontend.project_ids(new_ids)
                else:
                    import torch
                    talker = self.frontend.model.talker
                    with torch.inference_mode():
                        embedded = talker.get_text_embeddings()(torch.tensor([new_ids],device=self.frontend.device))
                        projected = talker.text_projection(embedded).detach().cpu().numpy()
                self.pending.extend(projected[:,i:i+1].copy() for i in range(projected.shape[1]))
        if self.pending:
            return self.pending.popleft()
        if not self.eos_sent:
            self.eos_sent = True
            return self.eos
        return self.pad

    def close(self):
        close = getattr(self.parts,'close',None)
        if close is not None:
            close()
