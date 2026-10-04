# Build from the repository root: docker build -f auth-web/Dockerfile -t me/auth-web:dev .
FROM mcr.microsoft.com/dotnet/sdk:10.0 AS build
WORKDIR /src
COPY auth-web .
RUN dotnet publish -c Release -o /app

FROM mcr.microsoft.com/dotnet/aspnet:10.0
WORKDIR /app
COPY --from=build /app .
USER 1000
ENTRYPOINT ["dotnet", "AuthWeb.dll"]
