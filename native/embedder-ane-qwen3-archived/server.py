"""Serves Qwen3 embeddings through Core ML on the Apple Neural Engine."""

import platform
import sys
import threading
from typing import TypedDict

MINIMUM_MACOS_VERSION = (13, 0)
MAX_REQUEST_TEXT_CHARS = 20_000


def require_supported_macos() -> None:
    version_text = platform.mac_ver()[0]
    version = tuple(int(part) for part in version_text.split(".")[:2]) if version_text else ()
    if sys.platform != "darwin" or version < MINIMUM_MACOS_VERSION:
        raise RuntimeError("native/embedder-ane requires macOS 13 or newer for CPU_AND_NE")


require_supported_macos()

import coremltools as ct
import numpy as np
from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from tokenizers import Tokenizer

MODEL_DIR = "models"
PAD_TOKEN = 151643
QUERY_INSTRUCTION = (
    "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery:"
)

app = FastAPI()


@app.exception_handler(HTTPException)
async def as_embedder_error(_request: Request, exc: HTTPException):
    return JSONResponse(status_code=exc.status_code, content={"error": exc.detail})


tokenizer = Tokenizer.from_file(f"{MODEL_DIR}/tokenizer.json")

MODEL_NAME = "Qwen/Qwen3-Embedding-0.6B"
single_neural_engine_lock = threading.Lock()


class ModelProfile(TypedDict):
    model: ct.models.MLModel
    seq_len: int


PROFILES: dict[str, ModelProfile] = {
    "short": {
        "model": ct.models.MLModel(
            f"{MODEL_DIR}/qwen3-b1_s128-8bit.mlpackage",
            compute_units=ct.ComputeUnit.CPU_AND_NE,
        ),
        "seq_len": 128,
    },
    "long": {
        "model": ct.models.MLModel(
            f"{MODEL_DIR}/qwen3-b1_s512-8bit.mlpackage",
            compute_units=ct.ComputeUnit.CPU_AND_NE,
        ),
        "seq_len": 512,
    },
}
MAX_TOKENS = PROFILES["long"]["seq_len"]


class EmbedRequest(BaseModel):
    text: str


class EmbedResponse(BaseModel):
    values: list[float]
    model_used: str
    truncated: bool


def require_text(request: EmbedRequest) -> str:
    if not request.text.strip():
        raise HTTPException(status_code=400, detail="text is empty")
    if len(request.text) > MAX_REQUEST_TEXT_CHARS:
        raise HTTPException(
            status_code=413,
            detail=f"text is longer than {MAX_REQUEST_TEXT_CHARS} characters",
        )
    return request.text


def run_embed(text: str) -> tuple[list[float], bool]:
    ids = tokenizer.encode(text).ids
    truncated = len(ids) > MAX_TOKENS
    if truncated:
        ids = ids[:MAX_TOKENS]
    profile = PROFILES["short"] if len(ids) <= PROFILES["short"]["seq_len"] else PROFILES["long"]
    seq_len = profile["seq_len"]

    pad_len = seq_len - len(ids)
    input_ids = [PAD_TOKEN] * pad_len + ids
    attention_mask = [0] * pad_len + [1] * len(ids)

    with single_neural_engine_lock:
        out = profile["model"].predict(
            {
                "input_ids": np.array([input_ids], dtype=np.int32),
                "attention_mask": np.array([attention_mask], dtype=np.int32),
            }
        )
    return np.array(out["embedding"])[0].tolist(), truncated


@app.get("/health")
def health():
    return "OK"


@app.post("/embed/document", response_model=EmbedResponse)
def embed_document(request: EmbedRequest):
    values, truncated = run_embed(require_text(request))
    return EmbedResponse(values=values, model_used=MODEL_NAME, truncated=truncated)


@app.post("/embed/query", response_model=EmbedResponse)
def embed_query(request: EmbedRequest):
    values, truncated = run_embed(f"{QUERY_INSTRUCTION} {require_text(request)}")
    return EmbedResponse(values=values, model_used=MODEL_NAME, truncated=truncated)
