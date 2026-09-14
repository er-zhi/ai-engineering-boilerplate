"""Host layout adapter for a strictly admitted, fixed-width ANE text projection."""
import numpy as np
import torch


class ANETextProjection(torch.nn.Module):
    def __init__(self, model, channels, width, output_channels):
        super().__init__()
        if min(channels, width, output_channels) <= 0:
            raise ValueError('Projection dimensions must be positive')
        self.model = model
        self.channels = channels
        self.width = width
        self.output_channels = output_channels

    def forward(self, embeddings):
        if embeddings.ndim != 3 or embeddings.shape[0] != 1 or embeddings.shape[2] != self.channels:
            raise ValueError('Expected text embeddings [1, tokens, channels]')
        source = embeddings.detach().cpu().float().numpy()
        if not np.isfinite(source).all():
            raise ValueError('Non-finite text embeddings')
        pieces = []
        for offset in range(0, source.shape[1], self.width):
            count = min(self.width, source.shape[1]-offset)
            batch = np.zeros((1, self.channels, 1, self.width), np.float32)
            batch[0, :, 0, :count] = source[0, offset:offset+count].T
            output = self.model.predict({'embeddings': batch})['projected']
            if output.shape != (1, self.output_channels, 1, self.width) or not np.isfinite(output).all():
                raise ValueError('Invalid ANE text projection output')
            pieces.append(output[:, :, 0, :count].transpose(0, 2, 1).copy())
        result = (np.concatenate(pieces, axis=1) if pieces else
            np.empty((1, 0, self.output_channels), np.float32))
        return torch.from_numpy(result).to(device=embeddings.device, dtype=embeddings.dtype)
