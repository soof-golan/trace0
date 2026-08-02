from uvicorn.workers import UvicornWorker


class AsyncioWorker(UvicornWorker):
    CONFIG_KWARGS = {"loop": "asyncio"}


class UvloopWorker(UvicornWorker):
    CONFIG_KWARGS = {"loop": "uvloop"}
