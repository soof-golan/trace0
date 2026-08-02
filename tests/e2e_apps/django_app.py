import asyncio
import os

import django
from django.conf import settings
from django.core.asgi import get_asgi_application
from django.http import JsonResponse
from django.urls import path

settings.configure(
    ALLOWED_HOSTS=["*"],
    DEBUG=False,
    LOGGING_CONFIG=None,
    ROOT_URLCONF=__name__,
    SECRET_KEY="trace0-e2e",
)
django.setup()


def django_endpoint_marker() -> int:
    return os.getpid()


async def work(request):
    await asyncio.sleep(0.02)
    return JsonResponse({"pid": django_endpoint_marker()})


urlpatterns = [path("work", work)]

asgi_app = get_asgi_application()
