import asyncio
import os

from fastapi import FastAPI

app = FastAPI()


def fastapi_endpoint_marker() -> int:
    return os.getpid()


@app.get("/work")
async def work() -> dict[str, int]:
    await asyncio.sleep(0.02)
    return {"pid": fastapi_endpoint_marker()}
